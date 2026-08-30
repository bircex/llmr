<!--
Say what changed and why. If a decision in here looks wrong without its reason, the reason
belongs in docs/DESIGN.md rather than in this description, where nobody will find it again.
-->

## What this changes

## Why

<!--
The checks below are the ones ROADMAP.md lists. The last two are not one check twice: a
doctest naming a feature gated item compiles under --all-features and nowhere else. Run them on the toolchain in
rust-toolchain.toml rather than whatever your laptop has: that file exists because the same
commands passed locally on 1.97 and failed on CI for months.
-->

## Checks

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-features --all-targets -- -D warnings`
- [ ] `cargo clippy --no-default-features --all-targets -- -D warnings`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`
- [ ] `cargo test --all-features`
- [ ] `cargo test`

## Two questions worth answering before merge

- [ ] **Does `docs/DESIGN.md` need a section?** It exists because the failure mode is
      somebody tidying a decision away, and this checkbox is where that gets caught. If this
      change makes a choice that would look wrong to a reader, write down what would break
      if it were reversed.
- [ ] **Does this change the public surface?** Narrowing after publish is breaking and
      widening never is. A new `pub` item is a promise; say so here if you added one.
