# Fuzz seed corpus

Hand-curated seed inputs for the fuzz harnesses. Mirror the bundled
test fixtures so the fuzzer starts from valid DTBs / DTBOs and
explores outward from there, rather than spending coverage on
header-validation noise.

The runtime corpus (`fuzz/corpus/{fuzz_parse,fuzz_overlay}/`) is
git-ignored. Copy seeds in before running:

```
cp fuzz/seeds/fuzz_parse/*   fuzz/corpus/fuzz_parse/
cp fuzz/seeds/fuzz_overlay/* fuzz/corpus/fuzz_overlay/
cargo +nightly fuzz run fuzz_parse
cargo +nightly fuzz run fuzz_overlay
```

Or pass the seeds dir directly:

```
cargo +nightly fuzz run fuzz_parse -- -seed_inputs=$(pwd)/fuzz/seeds/fuzz_parse
```
