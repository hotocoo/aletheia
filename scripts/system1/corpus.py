#!/usr/bin/env python3
"""Build System 1's console training corpus from the command table itself (ADR-187).

Nothing here lists commands or phrasings. The command table, the question wording and the way
options are built all come from `aletheiad console system1-schema` / `system1-questions`, i.e. from
the kernel's own `COMMANDS` through `console_ops`. Natural requests are PARAPHRASED from each
command's usage and help text by a System-2 model on an OpenAI-compatible endpoint, then kept only
if they carry the argument values they were asked to carry (so every gold label is an option the
console would really offer). A command added to the kernel is in the next corpus with no edit here.

    python3 scripts/system1/corpus.py --aletheiad aletheia/target/debug/aletheiad \\
        --endpoint http://127.0.0.1:8099 --out docs/evidence/system1/console-corpus.jsonl

Each output line is one training example on the serving wire:
    {"group": G, "state": REQUEST, "question": {...wire question...}, "answer": LABEL, "kind": ARG}
`group` identifies one paraphrase request; a trainer holds out whole groups, never single rows.
"""
import argparse
import json
import random
import re
import subprocess
import sys
import urllib.request

OBJECTS = ["manifesto", "poem", "notes", "report", "todo", "journal", "budget", "letter",
           "draft", "config", "readme", "ledger", "recipe", "plan", "diary", "memo", "story",
           "backup", "index", "scratch"]
TEXTS = ["hello from the model", "buy more coffee", "meeting at noon", "the build is green",
         "remember the milk", "ship it on friday", "call the plumber", "front", "error", "alpha",
         "status ok", "second draft done", "check the logs", "deploy tonight", "lunch"]


def llm(endpoint, prompt, temperature, max_tokens=800):
    body = json.dumps({
        "messages": [{"role": "user", "content": prompt}],
        "temperature": temperature,
        "max_tokens": max_tokens,
        "chat_template_kwargs": {"enable_thinking": False},
    }).encode()
    req = urllib.request.Request(endpoint.rstrip("/") + "/v1/chat/completions", data=body,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=300) as r:
        return json.load(r)["choices"][0]["message"].get("content") or ""


def clean(line):
    line = line.strip().strip("`").strip()
    line = re.sub(r"^\s*(?:[-*•]|\d+[.)])\s*", "", line)
    line = line.strip().strip('"').strip("'").strip()
    if not line or line.endswith(":") or len(line) > 160 or "\n" in line:
        return None
    return line


def fills(cmd, rng):
    """Concrete values for the typed arguments of one command, and a prompt phrase naming them."""
    vals, say = {}, []
    objs = rng.sample(OBJECTS, 3)
    nums = [str(rng.randint(1, 40)) for _ in range(3)]
    oi = ni = 0
    for i, a in enumerate(cmd["args"]):
        if a in SCHEMA["object_args"]:
            vals[a] = objs[oi]; oi += 1
            say.append("%s as the %s" % (vals[a], a))
        elif a in SCHEMA["number_args"]:
            if i < cmd["required"] or rng.random() < 0.6:
                vals[a] = nums[ni]; ni += 1
                say.append("%s as %s" % (vals[a], a.upper()))
        elif a == "text":
            t = rng.choice(TEXTS)
            if not (cmd["free_form_last"] and i == len(cmd["args"]) - 1):
                t = t.split()[0]
            vals[a] = t
            say.append('"%s" as the text' % t)
    return vals, say


PUNCT = ".,;:!?\"'`()"


def bare_words(request):
    """The console's own word split (`dual.rs::bare_words`): whitespace, wrapping punctuation off."""
    return [w for w in (t.strip(PUNCT) for t in request.split()) if w]


def keeps(request, cmd, vals):
    words = bare_words(request)
    for a, v in vals.items():
        if a == "text" and cmd["free_form_last"] and cmd["args"][-1] == "text":
            if not " ".join(words).endswith(v):
                return False
        elif v not in words:
            return False
    return True


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--aletheiad", required=True)
    ap.add_argument("--endpoint", required=True, help="OpenAI-compatible System-2 endpoint")
    ap.add_argument("--out", required=True)
    ap.add_argument("--groups", type=int, default=6, help="paraphrase requests per command")
    ap.add_argument("--per-group", type=int, default=8)
    ap.add_argument("--seed", type=int, default=186)
    ap.add_argument("--only", default="", help="comma-separated commands to (re)generate")
    ap.add_argument("--append", action="store_true", help="append to --out; groups continue after its last")
    a = ap.parse_args()
    rng = random.Random(a.seed)

    global SCHEMA
    SCHEMA = json.loads(subprocess.check_output([a.aletheiad, "console", "system1-schema"]))
    rows, stats = [], {}
    gid = 0
    if a.append:
        with open(a.out) as f:
            gid = 1 + max((json.loads(l)["group"] for l in f if l.strip()), default=-1)
    only = {x for x in a.only.split(",") if x}
    for cmd in SCHEMA["commands"]:
        if only and cmd["name"] not in only:
            continue
        kept = 0
        for g in range(a.groups):
            vals, say = fills(cmd, rng)
            want = "using " + ", ".join(say) if say else "(it takes no arguments)"
            prompt = (
                "An operator types plain-English requests to a small computer console. Write %d "
                "different short requests that mean exactly this console command:\n\n"
                "  %s  --  %s\n\n"
                "Write them %s. Vary the wording: some terse, some polite, some questions. Keep every "
                "value exactly as given.%s One request per line, no numbering, no quotes, no "
                "explanations, and never the command syntax itself."
                % (a.per_group, cmd["usage"], cmd["doc"], want,
                   " Each request must END with the exact text value, unquoted."
                   if cmd["free_form_last"] and "text" in cmd["args"] else "")
            )
            try:
                text = llm(a.endpoint, prompt, temperature=0.8 + 0.05 * g)
            except Exception as e:  # a failed call costs this group, not the corpus
                print("[corpus] %s group %d: %s" % (cmd["name"], g, e), file=sys.stderr)
                continue
            reqs = [r for r in (clean(l) for l in text.splitlines()) if r]
            # The literal command is a request too; it is what a person who knows the console types.
            literal = " ".join([cmd["name"]] + [vals[x] for x in cmd["args"] if x in vals])
            reqs.append(literal)
            for req in dict.fromkeys(reqs):
                if not keeps(req, cmd, vals):
                    continue
                objs = rng.sample(OBJECTS, rng.randint(1, 5))
                for x in ("name", "src"):
                    if x in vals and vals[x] not in objs and cmd["name"] not in ("write", "touch"):
                        objs.append(vals[x])
                rng.shuffle(objs)
                ctx = "  objects on this machine: " + ", ".join(
                    "%s (%d bytes)" % (o, rng.randint(1, 900)) for o in objs) + "\n"
                rows.append((gid, req, ctx, cmd["name"], vals))
                kept += 1
            gid += 1
        stats[cmd["name"]] = kept
        print("[corpus] %-10s %3d requests" % (cmd["name"], kept), file=sys.stderr, flush=True)

    # Every question through the exporter, so the corpus asks what the console asks.
    inp = "".join(json.dumps({"request": r, "context": c, "command": v}) + "\n" for _, r, c, v, _ in rows)
    out = subprocess.run([a.aletheiad, "console", "system1-questions"], input=inp, text=True,
                         capture_output=True, check=True).stdout.splitlines()
    n = 0
    with open(a.out, "a" if a.append else "w") as f:
        for (g, req, ctx, verb, vals), line in zip(rows, out):
            wire = json.loads(line)
            if "error" in wire:
                continue
            for arg, q in zip(wire["args"], wire["questions"]):
                ans = verb if arg == "command" else vals.get(arg)
                if ans is None or ans not in [o["label"] for o in q["options"]]:
                    continue
                f.write(json.dumps({"group": g, "state": wire["state"], "question": q,
                                    "answer": ans, "kind": arg}) + "\n")
                n += 1
    print("[corpus] %d examples from %d requests over %d commands -> %s"
          % (n, len(rows), len(stats), a.out), file=sys.stderr)
    thin = [k for k, v in stats.items() if v < 5]
    if thin:
        print("[corpus] THIN: %s" % ", ".join(thin), file=sys.stderr)


if __name__ == "__main__":
    main()
