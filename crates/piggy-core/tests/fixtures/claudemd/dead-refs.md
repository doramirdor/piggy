# Dead reference fixture

Input for the dead-reference detector. Half of these resolve inside this fixture
directory and half do not, so a test can prove the difference.

## References that resolve

- the empty fixture lives at empty.md
- the pair's first half is ./dup-pair/global.md
- the generated big one is oversized.md
- and [the pair](dup-pair/project.md) is a markdown link that lands

## References that do not

- the parser used to live in src/gone.rs
- the old notes moved to ./docs/removed.md
- the build script was scripts/old-build.sh
- [gone](docs/nope.md) is a markdown link to nothing
- the tail case is src/missing-tail.rs.
- an absolute one: /nonexistent-piggy-fixture-root/absolute.md

## Prose that is not a path

Read the docs at https://example.com/docs/thing.md or the mirror at
example.com/docs/host.md, neither of which is a file on this disk. Words with a
slash in them are prose, not references: and/or, read/write, Rust/TypeScript.
Placeholders are not references either: <project>/CLAUDE.md is a shape, and
.claude/rules/*.md is a glob, so neither can be resolved to one file.

Fenced examples are illustrations, not claims about this repository:

```
cargo run -- --config /fenced/example/path.rs
```

That fence is the last of it.
