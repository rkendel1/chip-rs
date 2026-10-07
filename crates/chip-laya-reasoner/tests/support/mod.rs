#![cfg(feature = "fixture")]
//! A tiny random-weight Laya checkpoint, written to a temp directory, so the real candle
//! load-and-infer path can be tested offline. The weights are reproducible pseudo-random
//! numbers: the verdicts it produces are meaningless and must never be read as model quality.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use candle_core::{DType, Device, Tensor};
use candle_nn::{VarBuilder, VarMap};
use candle_transformers::models::modernbert::{Config, ModernBert};
use tokenizers::Tokenizer;
use tokenizers::models::wordlevel::WordLevel;
use tokenizers::pre_tokenizers::whitespace::Whitespace;

const HIDDEN: usize = 64;

pub fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-laya-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes a complete checkpoint directory (config, encoder config, weights, tokenizer).
pub fn write_tiny_checkpoint(dir: &Path) {
    let device = Device::Cpu;

    // Tokenizer: a word-level vocabulary with Laya's special tokens.
    let mut vocab: HashMap<String, u32> = HashMap::new();
    for (i, t) in ["[PAD]", "[UNK]", "[CLS]", "[SEP]", "[MASK]"]
        .iter()
        .enumerate()
    {
        vocab.insert((*t).to_string(), i as u32);
    }
    for word in [
        "schema",
        "capability",
        "inputs",
        "evidence_state",
        "stale",
        "unknown",
        "valid",
        "true",
        "false",
        "tests.run",
        "build.run",
        "deploy.status",
        "compute.selftest",
        "CONTINUE",
        "ESCALATE",
    ] {
        let next = vocab.len() as u32;
        vocab.entry(word.to_string()).or_insert(next);
    }
    let vocab_size = vocab.len();
    let model = WordLevel::builder()
        .vocab(vocab.into_iter().collect())
        .unk_token("[UNK]".into())
        .build()
        .unwrap();
    let mut tokenizer = Tokenizer::new(model);
    tokenizer.with_pre_tokenizer(Some(Whitespace {}));
    std::fs::create_dir_all(dir.join("tokenizer")).unwrap();
    tokenizer
        .save(dir.join("tokenizer/tokenizer.json"), false)
        .unwrap();

    // Weights: a tiny ModernBERT encoder plus the decision head, with names and shapes
    // exactly as laya-decision loads them.
    let cfg = Config {
        vocab_size,
        hidden_size: HIDDEN,
        num_hidden_layers: 2,
        num_attention_heads: 2,
        intermediate_size: 128,
        max_position_embeddings: 512,
        layer_norm_eps: 1e-5,
        pad_token_id: 0,
        global_attn_every_n_layers: 3,
        global_rope_theta: 160000.0,
        local_attention: 128,
        local_rope_theta: 10000.0,
        classifier_config: None,
    };
    let varmap = VarMap::new();
    let vb = VarBuilder::from_varmap(&varmap, DType::F32, &device);
    ModernBert::load(vb.clone(), &cfg).expect("tiny encoder");
    let d = HIDDEN;
    let head_layers = 2;
    vb.get((3, d), "type_emb.weight").unwrap();
    for i in 0..head_layers {
        let p = format!("head.layers.{i}");
        vb.get((3 * d, d), &format!("{p}.self_attn.in_proj_weight"))
            .unwrap();
        vb.get(3 * d, &format!("{p}.self_attn.in_proj_bias"))
            .unwrap();
        vb.get((d, d), &format!("{p}.self_attn.out_proj.weight"))
            .unwrap();
        vb.get(d, &format!("{p}.self_attn.out_proj.bias")).unwrap();
        vb.get((4 * d, d), &format!("{p}.linear1.weight")).unwrap();
        vb.get(4 * d, &format!("{p}.linear1.bias")).unwrap();
        vb.get((d, 4 * d), &format!("{p}.linear2.weight")).unwrap();
        vb.get(d, &format!("{p}.linear2.bias")).unwrap();
        for n in ["norm1", "norm2"] {
            vb.get(d, &format!("{p}.{n}.weight")).unwrap();
            vb.get(d, &format!("{p}.{n}.bias")).unwrap();
        }
    }
    vb.get(d, "scorer.0.weight").unwrap();
    vb.get(d, "scorer.0.bias").unwrap();
    vb.get((d, d), "scorer.1.weight").unwrap();
    vb.get(d, "scorer.1.bias").unwrap();
    vb.get((1, d), "scorer.3.weight").unwrap();
    vb.get(1, "scorer.3.bias").unwrap();
    vb.get((256, d + 4), "act_head.0.weight").unwrap();
    vb.get(256, "act_head.0.bias").unwrap();
    vb.get((2, 256), "act_head.2.weight").unwrap();
    vb.get(2, "act_head.2.bias").unwrap();

    // Reproducible pseudo-random values (LCG), in a fixed name order.
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (((state >> 33) as f32) / (u32::MAX >> 1) as f32 - 0.5) * 0.2
    };
    let data = varmap.data().lock().unwrap();
    let mut names: Vec<_> = data.keys().cloned().collect();
    names.sort();
    for name in names {
        let var = &data[&name];
        // One-dimensional weights are layer-norm scales: start them at 1 so activations keep
        // a normal magnitude and the tiny model is sensitive to its input.
        let one_d_scale = var.dims().len() == 1 && name.ends_with(".weight");
        let values: Vec<f32> = (0..var.elem_count())
            .map(|_| if one_d_scale { 1.0 } else { next() })
            .collect();
        var.set(&Tensor::from_vec(values, var.shape().clone(), &device).unwrap())
            .unwrap();
    }
    drop(data);
    varmap.save(dir.join("model.safetensors")).unwrap();

    std::fs::write(
        dir.join("rl_agent_config.json"),
        r#"{"max_len":512,"head_max_len":192,"head_layers":2,"act_costs":{"act":1.0},"temperature":[1.0,1.0,1.0]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("config.json"),
        format!(
            r#"{{"vocab_size":{vocab_size},"hidden_size":{HIDDEN},"num_hidden_layers":2,"num_attention_heads":2,"intermediate_size":128,"max_position_embeddings":512,"pad_token_id":0,"global_attn_every_n_layers":3,"local_attention":128}}"#
        ),
    )
    .unwrap();
}
