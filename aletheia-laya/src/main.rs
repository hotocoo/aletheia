//! `aletheia-laya CHECKPOINT_DIR --serve-id ID [--port 8091] [--device cpu|metal]` (ADR-241): serve a
//! Laya checkpoint natively on the Aletheia decision wire, bound to 127.0.0.1. Same arguments as
//! the Python sidecar it replaces, so `aletheiad model serve` starts either one.

use aletheia_laya::model::Laya;
use aletheia_laya::wire::{handle, Decider};
use serde_json::Value;
use std::net::TcpListener;

impl Decider for Native {
    fn decide(&self, req: &Value) -> Result<Value, String> {
        self.0.decide(req)
    }
}
struct Native(Laya);

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let arg = |k: &str| {
        a.iter()
            .position(|x| x == k)
            .and_then(|i| a.get(i + 1))
            .cloned()
    };
    let (Some(dir), Some(serve_id)) =
        (a.get(1).filter(|d| !d.starts_with("--")), arg("--serve-id"))
    else {
        eprintln!(
            "usage: aletheia-laya CHECKPOINT_DIR --serve-id ID [--port 8091] [--device cpu|metal]"
        );
        std::process::exit(2);
    };
    let port = arg("--port").unwrap_or_else(|| "8091".into());
    let device = arg("--device").or_else(|| std::env::var("SYSTEM1_DEVICE").ok());
    // The GPU when there is one (as the reference runtime picks MPS), unless `cpu` is asked for.
    let want_gpu = match device.as_deref() {
        Some("cpu") => false,
        Some(_) => true,
        None => cfg!(target_os = "macos"),
    };
    let dev = if want_gpu {
        match candle_core::Device::new_metal(0) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[system1] no GPU device ({e}); running on cpu");
                candle_core::Device::Cpu
            }
        }
    } else {
        candle_core::Device::Cpu
    };
    // Elementwise kernels split across half the logical CPUs unless told otherwise (ADR-251): the
    // BLAS library runs its own threads beside them, and on a machine with efficiency cores using
    // every CPU was measured slower (16 threads 129 ms, 8 threads 112 ms per decision).
    if std::env::var_os("RAYON_NUM_THREADS").is_none() {
        let n = std::thread::available_parallelism().map_or(2, |n| n.get());
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads((n / 2).max(1))
            .build_global();
    }
    let t0 = std::time::Instant::now();
    let laya = match Laya::load(std::path::Path::new(dir), dev) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[system1] {serve_id}: the checkpoint in {dir} was refused: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "[system1] {serve_id} loaded natively from {dir} in {:.1} s",
        t0.elapsed().as_secs_f64()
    );
    let srv = match TcpListener::bind(format!("127.0.0.1:{port}")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[system1] cannot bind 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[system1] serving {serve_id} on 127.0.0.1:{port}");
    let native = Native(laya);
    for conn in srv.incoming().flatten() {
        handle(conn, &serve_id, &native);
    }
}
