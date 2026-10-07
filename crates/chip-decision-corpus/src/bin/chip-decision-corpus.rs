//! `chip-decision-corpus`: generate, verify and summarize the committed corpus.
//!
//! ```text
//! chip-decision-corpus generate [OUT_DIR]   # write decision-corpus-v1.jsonl and its manifest
//! chip-decision-corpus verify               # regenerate and compare with the committed files
//! chip-decision-corpus stats                # counts by family, split and holdout
//! ```

use std::path::{Path, PathBuf};

use chip_decision_corpus::{CORPUS_SCHEMA, Corpus, GENERATOR_VERSION, corpus_v1};

fn default_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

pub fn manifest(corpus: &Corpus) -> String {
    format!(
        "{{\n  \"schema\": \"{CORPUS_SCHEMA}\",\n  \"generator\": \"{GENERATOR_VERSION}\",\n  \"seed\": {},\n  \"cases\": {},\n  \"digest_sha256\": \"{}\"\n}}\n",
        corpus.config.seed,
        corpus.cases.len(),
        corpus.digest()
    )
}

fn main() {
    let corpus = corpus_v1();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("generate") => {
            let dir = args.get(2).map(PathBuf::from).unwrap_or_else(default_dir);
            std::fs::create_dir_all(&dir).expect("output directory");
            std::fs::write(dir.join("decision-corpus-v1.jsonl"), corpus.to_jsonl())
                .expect("corpus");
            std::fs::write(
                dir.join("decision-corpus-v1.manifest.json"),
                manifest(&corpus),
            )
            .expect("manifest");
            println!(
                "wrote {} ({} cases, sha256 {})",
                dir.display(),
                corpus.cases.len(),
                corpus.digest()
            );
        }
        Some("verify") => {
            let dir = default_dir();
            let same = std::fs::read_to_string(dir.join("decision-corpus-v1.jsonl"))
                .ok()
                .as_deref()
                == Some(&corpus.to_jsonl())
                && std::fs::read_to_string(dir.join("decision-corpus-v1.manifest.json"))
                    .ok()
                    .as_deref()
                    == Some(&manifest(&corpus));
            if same {
                println!(
                    "ok: the committed corpus is exactly what generator {GENERATOR_VERSION} produces"
                );
            } else {
                eprintln!(
                    "MISMATCH: regenerate with `chip-decision-corpus generate` (and bump the generator version if the generator changed)"
                );
                std::process::exit(1);
            }
        }
        Some("stats") => {
            println!(
                "{} cases, generator {GENERATOR_VERSION}, sha256 {}",
                corpus.cases.len(),
                corpus.digest()
            );
            let mut families = std::collections::BTreeMap::new();
            for c in &corpus.cases {
                let e = families.entry(c.family.name()).or_insert((0, 0));
                e.0 += 1;
                e.1 += usize::from(c.expected_continue);
            }
            for (family, (n, positives)) in families {
                println!("  {family:<12} {n:>4} cases, {positives:>3} continue");
            }
            for split in [
                chip_decision_corpus::Split::Train,
                chip_decision_corpus::Split::Validation,
                chip_decision_corpus::Split::HeldOut,
            ] {
                println!(
                    "  {:<12} {:>4}",
                    split.name(),
                    corpus.cases.iter().filter(|c| c.split == split).count()
                );
            }
        }
        _ => {
            eprintln!("usage: chip-decision-corpus generate [OUT_DIR] | verify | stats");
            std::process::exit(2);
        }
    }
}
