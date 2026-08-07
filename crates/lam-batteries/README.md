# `lam-batteries`

Optional network capability packs for [lam](../../README.md) agents. Each pack
installs typed namespaces without changing the model's one-tool (`eval`)
interface.

## Web search

Provider-native surfaces (not a unified facade):

| Namespace | Functions |
| --- | --- |
| `lam.exa` | `search`, `contents`, `context`, `answer`, `findSimilar` |
| `lam.parallel` | `search`, `extract` |

```rust,ignore
use lam::Lam;
use lam_batteries::{BatteriesPack, ExaConfig, ParallelConfig};

let batteries = BatteriesPack::builder()
    .exa(ExaConfig::from_api_key(std::env::var("EXA_API_KEY")?))
    .parallel(ParallelConfig::from_api_key(std::env::var("PARALLEL_API_KEY")?))
    .build()?;

let mut actor = Lam::builder(model)
    .namespaces(&batteries)
    .build()
    .actor("main")
    .build()
    .await?;
```

Namespaces are omitted when their provider is not configured. Function subsets
can be restricted through each provider config's `functions` allowlist.

HTTP stays in Rust; the isolate still has no ambient network authority.
