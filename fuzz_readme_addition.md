# Fuzz Testing Guide - Issue #362

## Running Fuzzers

### Run the full fuzzer (continuous)
```bash
cargo fuzz run black_scholes
```

### Reproduce a Single Corpus Entry

When a fuzzer crashes on a specific input, you can reproduce it by running against a single corpus file:

```bash
cargo fuzz run <target> <path-to-input>
```

**Example: Reproduce a crash from the existing corpus**

```bash
# Run the black_scholes fuzzer against a specific input file
cargo fuzz run black_scholes fuzz/corpus/black_scholes/at_expiry

# This will execute the fuzzer with just that one input, allowing you to:
# - Reproduce the exact crash
# - Debug with a fixed, minimal input
# - Verify the fix works
```

### Understanding Corpus Files

Corpus files in `fuzz/corpus/<target>/` contain minimal inputs that:
- Triggered interesting behavior
- Exposed edge cases
- Caused crashes (before they were fixed)

Each file represents one test case that will be re-run every time the fuzzer runs or during regression testing.

### After Fixing a Bug

1. Run the fuzzer against the crashing input to verify your fix
2. The corpus file remains in git so the fix doesn't regress
3. Continue fuzzing to find new edge cases

### Workflow Example

```bash
# 1. See that a corpus entry causes a crash
cargo fuzz run black_scholes fuzz/corpus/black_scholes/at_expiry
# → Crash! Stack trace shows issue in pricing logic

# 2. Fix the bug in src/pricing.rs
# ... edit code ...

# 3. Verify the fix
cargo fuzz run black_scholes fuzz/corpus/black_scholes/at_expiry
# → ✓ No crash!

# 4. Continue finding new crashes
cargo fuzz run black_scholes
```
