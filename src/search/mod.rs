//! Search — the staged, streaming, early-exit ranking pipeline.
//!
//! TODO(phase-1): Layers 1–3 (exact/prefix, abbreviation-aware fuzzy, path)
//! with an additive, `--explain`-able scorer. Layers 4–5 (live scan,
//! opportunistic extraction) arrive in phase 2.
