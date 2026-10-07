//! `chip-local-decision-train`: development-only training and reporting.
//!
//! ```text
//! chip-local-decision-train select            # grid search by leave-one-out on the dev split
//! chip-local-decision-train train [OUT_DIR]   # retrain with RECORDED_CONFIG; write artifact,
//!                                             # manifest and report (default: the models dir)
//! chip-local-decision-train report            # print the full report
//! ```

use std::path::{Path, PathBuf};

use chip_local_decision_train::pr25;
use chip_local_decision_train::report::{full_report, manifest_json};
use chip_local_decision_train::{
    RECORDED_CONFIG, labeled_corpus, leave_one_out, select, split, train_final,
};

fn default_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../chip-local-decision/models")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cases = labeled_corpus();
    let cfg = RECORDED_CONFIG;
    match args.get(1).map(String::as_str) {
        Some("select") => {
            let sp = split(&cases, cfg.seed);
            let (best, result) = select(&cases, &sp.development(), &cfg);
            println!("selected: {best:?}");
            println!(
                "{}",
                result.render("Leave-one-out (dev) at the selected configuration")
            );
        }
        Some("train") => {
            let dir = args.get(2).map(PathBuf::from).unwrap_or_else(default_dir);
            std::fs::create_dir_all(&dir).expect("output directory");
            let (model, sp) = train_final(&cases, &cfg);
            let dev_loo = leave_one_out(&cases, &sp.development(), &cfg);
            std::fs::write(dir.join("local-decision-v1.bin"), model.to_bytes()).expect("artifact");
            std::fs::write(
                dir.join("local-decision-v1.manifest.json"),
                manifest_json(&cases, &sp, &cfg, &model, &dev_loo),
            )
            .expect("manifest");
            std::fs::write(
                dir.join("local-decision-v1.report.txt"),
                full_report(&cases, &sp, &cfg, &model),
            )
            .expect("report");
            println!("wrote {}", dir.display());
        }
        Some("report") => {
            let (model, sp) = train_final(&cases, &cfg);
            print!("{}", full_report(&cases, &sp, &cfg, &model));
        }
        Some("pr25-train") => {
            let dir = args.get(2).map(PathBuf::from).unwrap_or_else(default_dir);
            std::fs::create_dir_all(&dir).expect("output directory");
            let cases25 = pr25::load();
            let model = pr25::train(&cases25, &cfg);
            let unseen = pr25::unseen_evaluation(&cases25, &model);
            std::fs::write(dir.join("local-decision-pr25.bin"), model.to_bytes())
                .expect("artifact");
            std::fs::write(
                dir.join("local-decision-pr25.manifest.json"),
                pr25::manifest(&cases25, &model, &cfg, &unseen),
            )
            .expect("manifest");
            std::fs::write(
                dir.join("local-decision-pr25.report.txt"),
                pr25::report(&cases25, &model, &cfg),
            )
            .expect("report");
            println!("wrote {}", dir.display());
        }
        Some("pr25-report") => {
            let cases25 = pr25::load();
            let model = pr25::train(&cases25, &cfg);
            print!("{}", pr25::report(&cases25, &model, &cfg));
        }
        _ => {
            eprintln!(
                "usage: chip-local-decision-train select | train [OUT_DIR] | report | pr25-train [OUT_DIR] | pr25-report"
            );
            std::process::exit(2);
        }
    }
}
