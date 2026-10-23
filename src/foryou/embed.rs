//! Small local text embeddings (Model2Vec "static" models such as potion-base-8M).
//!
//! A static model is a vocabulary and one vector per word piece: a title's vector is the
//! average of its pieces' vectors, normalised. That needs no neural network at run time, so
//! this reads the model files directly instead of pulling in a tokenizer library: a BERT-style
//! WordPiece split and a `safetensors` matrix of f32s. About 8,000 titles a second on one core.

use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The model For You uses: potion-base-8M by Minish (MIT), pinned to one revision and checked
/// against these hashes, so a changed upload can't slip in.
const REPO: &str = "minishlab/potion-base-8M";
const REVISION: &str = "bf8b056651a2c21b8d2565580b8569da283cab23";
const FILES: [(&str, &str); 2] = [
    ("tokenizer.json", "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50"),
    ("model.safetensors", "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2"),
];

/// Where the model lives (shared by all profiles).
pub fn model_dir() -> PathBuf {
    crate::db::data_dir().join("models").join("potion-base-8M")
}

pub fn installed() -> bool {
    FILES.iter().all(|(f, _)| model_dir().join(f).is_file())
}

/// Download the model once (about 30 MB, through the configured proxy) and check its hashes.
pub fn install() -> Result<()> {
    let dir = model_dir();
    std::fs::create_dir_all(&dir)?;
    for (name, sha) in FILES {
        let dest = dir.join(name);
        if dest.is_file() {
            continue;
        }
        let url = format!("https://huggingface.co/{REPO}/resolve/{REVISION}/{name}");
        // Over Tor, Hugging Face sometimes turns an exit away (429); another exit usually works.
        let mut tries = 0;
        let bytes = loop {
            tries += 1;
            let r = crate::yt::agent_tagged(&format!("model-{tries}"), std::time::Duration::from_secs(300))
                .get(&url)
                .call()
                .and_then(|mut r| r.body_mut().with_config().limit(64 * 1024 * 1024).read_to_vec());
            match r {
                Ok(b) => break b,
                Err(e) if tries >= 3 => return Err(e.into()),
                Err(_) => std::thread::sleep(std::time::Duration::from_secs(2)),
            }
        };
        let got: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        if got != sha {
            bail!("{name} didn't match its expected checksum, so it wasn't used");
        }
        let tmp = dest.with_extension("part");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &dest)?;
    }
    Ok(())
}

/// The installed model, if there is one.
pub fn load_installed() -> Option<Model> {
    installed().then(|| Model::load(&model_dir()).ok()).flatten()
}

/// Your strongest searches and watched titles (up to `n`), embedded, each with its weight's square
/// root scaled to 0..1, so small interests still count.
pub fn interests(m: &Model, p: &super::profile::Profile, n: usize) -> Vec<(Vec<f32>, f32)> {
    let mut t: Vec<&(String, f64)> = p.texts.iter().filter(|(_, w)| *w > 0.0).collect();
    t.sort_by(|a, b| b.1.total_cmp(&a.1));
    t.truncate(n);
    let top = t.first().map_or(1.0, |x| x.1.sqrt());
    t.into_iter().map(|(s, w)| (m.encode(s), (w.sqrt() / top) as f32)).filter(|(e, _)| e.iter().any(|x| *x != 0.0)).collect()
}

/// How close a title's embedding is to its nearest interest (PinnerSage-style: best match, not
/// the average, so one strong interest isn't washed out by the rest).
pub fn nearest(interests: &[(Vec<f32>, f32)], emb: &[f32]) -> f64 {
    interests.iter().map(|(e, w)| dot(e, emb) * (0.5 + 0.5 * w)).fold(0.0f32, f32::max) as f64
}

pub struct Model {
    vocab: HashMap<String, u32>,
    unk: Option<u32>,
    dim: usize,
    data: Vec<f32>,
}

impl Model {
    /// Load `tokenizer.json` and `model.safetensors` from `dir`.
    pub fn load(dir: &Path) -> Result<Model> {
        let tok: Value = serde_json::from_slice(&std::fs::read(dir.join("tokenizer.json"))?)?;
        let m = &tok["model"];
        if m["type"] != "WordPiece" {
            bail!("only WordPiece tokenizers are supported");
        }
        let vocab: HashMap<String, u32> = m["vocab"]
            .as_object()
            .ok_or_else(|| anyhow!("tokenizer has no vocab"))?
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_u64()? as u32)))
            .collect();
        let unk = m["unk_token"].as_str().and_then(|u| vocab.get(u).copied());

        let bytes = std::fs::read(dir.join("model.safetensors"))?;
        let n = u64::from_le_bytes(bytes.get(..8).ok_or_else(|| anyhow!("short model file"))?.try_into()?) as usize;
        let header: Value = serde_json::from_slice(bytes.get(8..8 + n).ok_or_else(|| anyhow!("bad model header"))?)?;
        let t = header
            .as_object()
            .and_then(|o| o.iter().find(|(k, _)| *k != "__metadata__"))
            .map(|(_, v)| v)
            .ok_or_else(|| anyhow!("no tensor"))?;
        if t["dtype"] != "F32" {
            bail!("only F32 models are supported");
        }
        let shape: Vec<usize> = t["shape"].as_array().into_iter().flatten().filter_map(|x| x.as_u64().map(|x| x as usize)).collect();
        let (rows, dim) = match shape[..] {
            [r, d] => (r, d),
            _ => bail!("expected a 2-D matrix"),
        };
        let off: Vec<usize> = t["data_offsets"].as_array().into_iter().flatten().filter_map(|x| x.as_u64().map(|x| x as usize)).collect();
        let raw = bytes.get(8 + n + off[0]..8 + n + off[1]).ok_or_else(|| anyhow!("model data out of range"))?;
        if raw.len() != rows * dim * 4 || vocab.values().any(|i| *i as usize >= rows) {
            bail!("model and vocabulary don't match");
        }
        let data = raw.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        Ok(Model { vocab, unk, dim, data })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// The text's vector (unit length), or all zeros when no word is known.
    pub fn encode(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        let mut n = 0;
        for id in self.pieces(text) {
            let row = &self.data[id as usize * self.dim..(id as usize + 1) * self.dim];
            for (a, b) in v.iter_mut().zip(row) {
                *a += b;
            }
            n += 1;
        }
        if n > 0 {
            normalize(&mut v);
        }
        v
    }

    /// WordPiece ids for `text`, unknown pieces left out.
    fn pieces(&self, text: &str) -> Vec<u32> {
        let mut out = Vec::new();
        for word in pre_tokenize(&normalize_text(text)) {
            if word.chars().count() > 100 {
                continue;
            }
            let chars: Vec<char> = word.chars().collect();
            let mut start = 0;
            let mut ids = Vec::new();
            let mut ok = true;
            while start < chars.len() {
                let mut end = chars.len();
                let mut found = None;
                while start < end {
                    let piece: String = chars[start..end].iter().collect();
                    let key = if start > 0 { format!("##{piece}") } else { piece };
                    if let Some(id) = self.vocab.get(&key) {
                        found = Some(*id);
                        break;
                    }
                    end -= 1;
                }
                match found {
                    Some(id) => {
                        ids.push(id);
                        start = end;
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                out.extend(ids.into_iter().filter(|i| Some(*i) != self.unk));
            }
        }
        out
    }
}

pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// BERT's normaliser: lowercase, drop control characters, and strip the common Latin accents.
fn normalize_text(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
            'ç' | 'č' => 'c',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ě' => 'e',
            'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
            'ñ' | 'ň' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
            'š' => 's',
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' => 'u',
            'ý' | 'ÿ' => 'y',
            'ž' => 'z',
            c => c,
        })
        .collect()
}

/// BERT's pre-tokeniser: split on whitespace; punctuation and CJK characters stand alone.
fn pre_tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        let cjk = matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F);
        if c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else if c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace()) || cjk {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            out.push(c.to_string());
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_bert() {
        assert_eq!(pre_tokenize("how-to: e30 turbo!"), ["how", "-", "to", ":", "e30", "turbo", "!"]);
        assert_eq!(normalize_text("Café ÉLAN"), "cafe elan");
    }

    #[test]
    fn wordpiece_and_mean() {
        let vocab: HashMap<String, u32> =
            [("[UNK]", 0), ("turbo", 1), ("e", 2), ("##30", 3)].into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        let data = vec![0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 1.0];
        let m = Model { vocab, unk: Some(0), dim: 2, data };
        assert_eq!(m.pieces("Turbo E30 zzz"), vec![1, 2, 3]);
        let v = m.encode("turbo");
        assert!((v[0] - 1.0).abs() < 1e-6 && v[1].abs() < 1e-6);
        assert_eq!(m.encode("zzz"), vec![0.0, 0.0]);
    }
}
