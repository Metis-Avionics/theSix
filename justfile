# Local gates for theSix.
#
#   just              # the default pass: every mandatory gate, in contract order
#   just gates        # same thing, spelled out
#   just list         # what runs, and why
#   just quick        # fmt + contract + check + the fast test layers
#   just perf         # the deferred performance gates
#   just soak         # the deferred soak gates
#   just fuzz         # nightly fuzz targets (needs cargo-fuzz)
#   just loom         # exhaustive control-plane interleavings
#   just <gate>       # any single gate by name, e.g. `just clippy`
#
# Every recipe delegates to `cargo xtask`, which reads `theSix.toml`. There is no
# second copy of the gate list: adding a gate to the contract adds it here.

default:
    @cargo xtask gates

# Show the toolchain accelerations that are actually in play.
toolchain:
    @cargo xtask toolchain

contract:
    @cargo xtask run contract

list:
    @cargo xtask list

deferred:
    @cargo xtask deferred

layers:
    @cargo xtask layers

# Everything, including the nightly and long-running gates.
all:
    @cargo xtask run all

# The fast feedback loop: everything that finishes in well under a minute on a
# warm cache. Use this while editing; use `just` before pushing.
quick:
    @cargo xtask run fmt
    @cargo xtask run contract
    @cargo xtask run check
    @cargo xtask run unit
    @cargo xtask run negative

# Show what a gate would execute without running it.
show gate:
    @cargo xtask show {{gate}}

# Run one gate by name.
gate name:
    @cargo xtask run {{name}}

# Print the exact argv for every gate, for review.
plan:
    @cargo xtask gates --dry-run

clean:
    cargo clean

# Clear the sccache as well as the target dir.
distclean: clean
    sccache --stop-server || true
    rm -rf ~/.cache/sccache
