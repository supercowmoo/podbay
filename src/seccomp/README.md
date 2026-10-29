# Vendored seccomp profile

`moby-default.json` is Docker's default seccomp profile, copied unchanged
from [moby/profiles](https://github.com/moby/profiles) at commit
`4b8214fc4971e06172938a1f1554ccff6647ee80`
(`seccomp/default.json`, sha256
`6416b47770785a41ac59073cdc77d9fe98517df2799dc83ef207e622de3053f6`).

It is licensed under the Apache License, Version 2.0; see
<https://github.com/moby/profiles/blob/main/LICENSE>.

podbay doesn't edit this file. `src/seccomp.rs` reads it and applies its
changes in code, so updating means replacing the file and running the
tests, which check that upstream still has the shape podbay expects.
