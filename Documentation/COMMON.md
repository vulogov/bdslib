# common module

The `common` module provides shared utilities used across all bdslib engines. It has two sub-modules: `error` and `math`.

```rust
use bdslib::common::error::{Result, Error, err_msg};
use bdslib::common::math::{cosine_similarity, dot_product, l2_norm, normalize, euclidean_distance, squared_euclidean};
```

---

## common::error

Shared error and result types used by every bdslib module.

### `Result<T>`

```rust
pub type Result<T> = std::result::Result<T, easy_error::Error>;
```

All public methods in `StorageEngine`, `FTSEngine`, and `EmbeddingEngine` return this type. Using a single alias across the crate means errors propagate with `?` without any conversion between module-specific result types.

### `Error`

Re-export of `easy_error::Error`. Carries a human-readable message and an optional boxed cause:

```rust
use bdslib::common::error::Error;
```

### `err_msg`

Re-export of `easy_error::err_msg`. Constructs an `Error` from any string:

```rust
use bdslib::common::error::err_msg;

return Err(err_msg("something went wrong"));
return Err(err_msg(format!("value out of range: {val}")));
```

---

## common::math

Pure vector arithmetic functions. All inputs are `&[f32]`; all fallible functions return `bdslib::common::error::Result<T>`.

None of these functions allocate unless explicitly stated (i.e., `normalize`).

---

### `dot_product`

```rust
pub fn dot_product(a: &[f32], b: &[f32]) -> Result<f32>
```

Returns the inner product `Σ aᵢ·bᵢ`.

Returns `Err` if `a` and `b` have different lengths.

```rust
let d = dot_product(&[1.0, 2.0], &[3.0, 4.0])?; // 11.0
```

---

### `l2_norm`

```rust
pub fn l2_norm(v: &[f32]) -> f32
```

Returns the Euclidean (L2) norm `√(Σ vᵢ²)`. Returns `0.0` for an empty slice. Never returns `Err`.

```rust
let n = l2_norm(&[3.0, 4.0]); // 5.0
```

---

### `normalize`

```rust
pub fn normalize(v: &[f32]) -> Result<Vec<f32>>
```

Returns a unit-length copy of `v` (each element divided by `l2_norm(v)`).

Returns `Err` if `v` is empty or is the zero vector.

```rust
let u = normalize(&[3.0, 0.0, 4.0])?; // [0.6, 0.0, 0.8]
```

---

### `cosine_similarity`

```rust
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Result<f32>
```

Returns the cosine similarity `dot(a,b) / (‖a‖·‖b‖)` in the range `[-1.0, 1.0]`.

Returns `Err` on dimension mismatch, empty input, or a zero-norm vector (undefined cosine).

| Score | Meaning |
|---|---|
| `1.0` | Identical direction |
| `0.0` | Orthogonal |
| `-1.0` | Opposite direction |

```rust
use bdslib::common::math::cosine_similarity;

let sim = cosine_similarity(&e1, &e2)?;
```

This is the same computation as `EmbeddingEngine::compare_embeddings` — use this form when you don't have an `EmbeddingEngine` in scope.

---

### `euclidean_distance`

```rust
pub fn euclidean_distance(a: &[f32], b: &[f32]) -> Result<f32>
```

Returns the Euclidean distance `√(Σ (aᵢ−bᵢ)²)`.

Returns `Err` if `a` and `b` have different lengths.

```rust
let d = euclidean_distance(&[0.0, 0.0], &[3.0, 4.0])?; // 5.0
```

---

### `squared_euclidean`

```rust
pub fn squared_euclidean(a: &[f32], b: &[f32]) -> Result<f32>
```

Returns `Σ (aᵢ−bᵢ)²` — the squared Euclidean distance. Cheaper than `euclidean_distance` when you only need to compare distances (avoids the `sqrt`).

Returns `Err` if `a` and `b` have different lengths.

```rust
let d2 = squared_euclidean(&[1.0, 2.0], &[4.0, 6.0])?; // 25.0
```

---

## Error handling

All fallible functions in `common::math` return `bdslib::common::error::Result<T>` and use `?` cleanly:

```rust
use bdslib::common::math::{cosine_similarity, normalize};
use bdslib::common::error::Result;

fn nearest(query: &[f32], corpus: &[&[f32]]) -> Result<usize> {
    let q = normalize(query)?;
    let mut best = (usize::MAX, f32::NEG_INFINITY);
    for (i, doc) in corpus.iter().enumerate() {
        let sim = cosine_similarity(&q, doc)?;
        if sim > best.1 {
            best = (i, sim);
        }
    }
    Ok(best.0)
}
```
