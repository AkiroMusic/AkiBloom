//! AkiBloom offline renderer — grow a variant without a host or GUI.
//!
//! Usage:
//!   akibloom_demo <seed_a.wav> [seed_b.wav] <out.wav> [--alpha 0.7]
//!                 [--side a|b] [--tag 0|1|2] [--amount 0.3] [--seed 42]
//!
//! With two seeds the source (default a) is grafted toward the other seed's
//! timbre by `--alpha`; with one seed only the variation transform applies.
//! The output loops seamlessly (loop point pre-crossfaded).

use std::path::PathBuf;
use std::process::ExitCode;

use akibloom::graft::VariateState;
use akibloom::render::{render_variant, write_variant, RenderJob};
use akibloom::seeds::{self, Slot};

fn usage() -> ExitCode {
    eprintln!(
        "用法 Usage: akibloom_demo <seed_a.wav> [seed_b.wav] <out.wav> \
[--alpha 0.7] [--side a|b] [--tag 0|1|2] [--amount 0.3] [--seed 42]"
    );
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut positional: Vec<PathBuf> = Vec::new();
    let mut alpha = 0.7f32;
    let mut side = Slot::A;
    let mut tag = 0u8;
    let mut amount = 0.0f32;
    let mut seed = 1u64;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--alpha" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => alpha = v,
                None => return usage(),
            },
            "--side" => match args.next().as_deref() {
                Some("a") => side = Slot::A,
                Some("b") => side = Slot::B,
                _ => return usage(),
            },
            "--tag" => match args.next().and_then(|v| v.parse::<u8>().ok()) {
                Some(v) if v <= 2 => tag = v,
                _ => return usage(),
            },
            "--amount" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => amount = v,
                None => return usage(),
            },
            "--seed" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => seed = v,
                None => return usage(),
            },
            other => positional.push(PathBuf::from(other)),
        }
    }
    if positional.len() < 2 || positional.len() > 3 {
        return usage();
    }
    let output = positional.pop().unwrap();
    let seed_b = if positional.len() == 2 {
        Some(positional.pop().unwrap())
    } else {
        None
    };
    let seed_a = positional.pop().unwrap();

    let load = |path: PathBuf, slot: Slot| match seeds::load_seed(&path) {
        Ok(s) => {
            println!("种子 {slot:?}: {}", s.info_line());
            Some(s)
        }
        Err(e) => {
            eprintln!("种子 {slot:?} 加载失败: {e}");
            None
        }
    };
    let a = load(seed_a, Slot::A);
    let b = seed_b.and_then(|p| load(p, Slot::B));

    let (source, donor) = match side {
        Slot::A => (a, b),
        Slot::B => (b, a),
    };
    let Some(source) = source else {
        eprintln!("源种子（--side）未加载，无事可做");
        return usage();
    };

    let variate = if amount > 0.0 {
        Some(VariateState { tag, amount, seed })
    } else {
        None
    };
    let variant = render_variant(RenderJob {
        source: &source,
        donor: donor.as_ref(),
        alpha,
        variate,
    });

    match write_variant(&output, &variant) {
        Ok(()) => {
            println!(
                "已写出 {} (α={alpha}, side={side:?}, tag={tag}, amount={amount}, seed={seed})",
                output.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("写出失败: {e}");
            ExitCode::FAILURE
        }
    }
}
