#!/usr/bin/env python3
"""Fine-tune a `laya`-backend System-1 checkpoint on Aletheia's console corpus (ADR-187).

    python3 scripts/system1/laya_finetune.py BASE_DIR docs/evidence/system1/console-corpus.jsonl \\
        OUT_DIR [--epochs 4] [--unfreeze 0] [--device mps]

* Examples are the corpus's wire questions, rendered by the backend's own `build_sequence` through
  the SAME wire-to-backend conversion the sidecar uses (`laya_server.to_laya`), so training and
  serving see identical token sequences. Options are shuffled every epoch: the head cannot learn a
  position.
* Encoder frozen by default (the decision head, type embedding and scorer train); `--unfreeze N`
  also trains the top N encoder layers.
* Whole paraphrase GROUPS are held out, never single rows, and the held-out groups are split in
  two: one half refits the per-option-count temperatures (calibration is what the escalation
  threshold stands on), the other half is the untouched test set the report is computed on.
* The report is written to OUT_DIR/system1-finetune.json and printed.
"""
import argparse
import json
import math
import os
import random
import shutil
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from laya_server import to_laya  # noqa: E402  (the sidecar's own conversion)


def load_rows(path):
    """Corpus rows the console would really ask: a choice with fewer than two options is never
    asked (ADR-188), so it is not a training example either."""
    with open(path) as f:
        rows = [json.loads(l) for l in f if l.strip()]
    return [r for r in rows if len(r["question"].get("options") or []) >= 2]


def split_groups(rows, holdout, seed):
    groups = sorted({r["group"] for r in rows})
    random.Random(seed).shuffle(groups)
    n = int(round(len(groups) * holdout))
    calib = set(groups[: n // 2])
    test = set(groups[n // 2: n])
    tr = [r for r in rows if r["group"] not in calib | test]
    return tr, [r for r in rows if r["group"] in calib], [r for r in rows if r["group"] in test]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("base")
    ap.add_argument("corpus")
    ap.add_argument("out")
    ap.add_argument("--epochs", type=int, default=4)
    ap.add_argument("--lr", type=float, default=1e-4)
    ap.add_argument("--batch", type=int, default=16)
    ap.add_argument("--unfreeze", type=int, default=0)
    ap.add_argument("--holdout", type=float, default=0.2)
    ap.add_argument("--seed", type=int, default=187)
    ap.add_argument("--device", default=None)
    ap.add_argument("--threshold", type=float, default=0.9, help="report accuracy/coverage at this confidence")
    a = ap.parse_args()

    import numpy as np
    import torch
    import laya
    from laya.common import QTYPES, build_sequence, collate_items, confidence_from_probs, temp_bucket

    rng = random.Random(a.seed)
    torch.manual_seed(a.seed)
    rows = load_rows(a.corpus)
    train, calib, test = split_groups(rows, a.holdout, a.seed)
    print("[finetune] %d rows: %d train, %d calibration, %d test" % (len(rows), len(train), len(calib), len(test)),
          flush=True)

    agent = laya.load(a.base, device=a.device)
    model, dev, tok, cfg = agent.model, agent.device, agent.tok, agent.cfg
    max_len, head_max = cfg.get("max_len", 512), cfg.get("head_max_len", 192)

    def item(r, shuffle):
        q = agent._to_internal(to_laya(r["question"]))
        keys = list(q["crit"].keys())
        order = list(range(len(keys)))
        if shuffle:
            rng.shuffle(order)
        ids, markers = build_sequence(tok, r["state"], q, max_len, head_max, option_order=order)
        if len(markers) != len(keys):
            return None
        return {"ids": ids, "markers": markers, "qtype": QTYPES["choice"],
                "label": order.index(keys.index(r["answer"])), "k": len(keys)}

    for p in model.encoder.parameters():
        p.requires_grad_(False)
    if a.unfreeze > 0:
        layers = model.encoder.layers
        for layer in layers[len(layers) - a.unfreeze:]:
            for p in layer.parameters():
                p.requires_grad_(True)
    params = [p for p in model.parameters() if p.requires_grad]
    print("[finetune] training %.1f M parameters on %s" % (sum(p.numel() for p in params) / 1e6, dev), flush=True)
    opt = torch.optim.AdamW(params, lr=a.lr, weight_decay=0.01)

    def logits_of(items):
        b = collate_items([items], tok.pad_token_id)
        lg, _ = model(b["input_ids"].to(dev), b["attention_mask"].to(dev), b["marker_pos"].to(dev),
                      b["marker_mask"].to(dev), b["qtype"].to(dev))
        return lg, b["label"].to(dev)

    t0 = time.time()
    for ep in range(a.epochs):
        model.train()
        if a.unfreeze == 0:
            model.encoder.eval()
        items = [x for x in (item(r, True) for r in train) if x]
        rng.shuffle(items)
        tot, n = 0.0, 0
        for i in range(0, len(items), a.batch):
            lg, lab = logits_of(items[i: i + a.batch])
            loss = torch.nn.functional.cross_entropy(lg, lab)
            opt.zero_grad()
            loss.backward()
            torch.nn.utils.clip_grad_norm_(params, 1.0)
            opt.step()
            tot += loss.item()
            n += 1
        print("[finetune] epoch %d loss %.4f (%.0f s)" % (ep + 1, tot / max(1, n), time.time() - t0), flush=True)

    model.eval()

    @torch.no_grad()
    def raw(rs):
        out = []
        for i in range(0, len(rs), a.batch):
            its = [x for x in (item(r, False) for r in rs[i: i + a.batch]) if x]
            if not its:
                continue
            lg, lab = logits_of(its)
            for j, it in enumerate(its):
                out.append((lg[j, : it["k"]].float().cpu().numpy(), it["label"], it["k"]))
        return out

    # Temperature per option-count bucket, fitted on the calibration half by NLL grid search.
    temps = {}
    buckets = {}
    for z, lab, k in raw(calib):
        buckets.setdefault(temp_bucket(QTYPES["choice"], k), []).append((z, lab))
    grid = [0.05 * i for i in range(1, 101)]
    for b, xs in buckets.items():
        def nll(t):
            s = 0.0
            for z, lab in xs:
                zz = z / t
                zz = zz - zz.max()
                s -= zz[lab] - math.log(np.exp(zz).sum())
            return s
        temps[b] = min(grid, key=nll)
    cfg = dict(cfg)
    cfg["temperature_by_options"] = {**cfg.get("temperature_by_options", {}), **temps}

    # The report, on the untouched test half.
    rs = [r for r in test if item(r, False)]
    res = raw(rs)
    per, conf_ok, bins = {}, [], []
    for (z, lab, k), r in zip(res, rs):
        t = cfg["temperature_by_options"].get(temp_bucket(QTYPES["choice"], k), 1.0)
        p = np.exp((z - z.max()) / t)
        p /= p.sum()
        c = float(confidence_from_probs(p, k))
        ok = int(p.argmax()) == lab
        kd = per.setdefault(r["kind"], [0, 0])
        kd[0] += ok
        kd[1] += 1
        conf_ok.append((c, ok))
    sure = [(c, ok) for c, ok in conf_ok if c >= a.threshold]
    report = {
        "base": os.path.abspath(a.base),
        "corpus": a.corpus,
        "rows": {"train": len(train), "calibration": len(calib), "test": len(test)},
        "epochs": a.epochs, "lr": a.lr, "unfreeze": a.unfreeze, "seed": a.seed,
        "temperatures": temps,
        "test_accuracy": {k: round(v[0] / v[1], 4) for k, v in sorted(per.items())},
        "test_accuracy_all": round(sum(ok for _, ok in conf_ok) / max(1, len(conf_ok)), 4),
        "threshold": a.threshold,
        "coverage_at_threshold": round(len(sure) / max(1, len(conf_ok)), 4),
        "accuracy_at_threshold": round(sum(ok for _, ok in sure) / max(1, len(sure)), 4),
        "wrong_and_sure": sum(1 for _, ok in sure if not ok),
        "minutes": round((time.time() - t0) / 60, 1),
    }

    tmp = a.out.rstrip("/") + ".partial"
    shutil.rmtree(tmp, ignore_errors=True)
    os.makedirs(tmp)
    for sub in ("tokenizer", "encoder"):
        if os.path.isdir(os.path.join(a.base, sub)):
            shutil.copytree(os.path.join(a.base, sub), os.path.join(tmp, sub))
    # Saved in the base checkpoint's own dtype: an fp32 copy of an fp16 model doubles the download
    # and the disk for nothing the serving path can use.
    from safetensors import safe_open
    from safetensors.torch import save_model
    with safe_open(os.path.join(a.base, "model.safetensors"), "pt") as f:
        base_dtype = f.get_tensor(next(iter(f.keys()))).dtype
    save_model(model.cpu().to(base_dtype), os.path.join(tmp, "model.safetensors"))
    cfg["model_name"] = "aletheia-console-system1"
    cfg["fine_tuned_from"] = cfg.get("fine_tuned_from", []) + [os.path.abspath(a.base)]
    cfg["aletheia_finetune"] = report
    with open(os.path.join(tmp, "rl_agent_config.json"), "w") as f:
        json.dump(cfg, f, indent=2)
    with open(os.path.join(tmp, "system1-finetune.json"), "w") as f:
        json.dump(report, f, indent=2)
    shutil.rmtree(a.out, ignore_errors=True)
    os.rename(tmp, a.out)
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
