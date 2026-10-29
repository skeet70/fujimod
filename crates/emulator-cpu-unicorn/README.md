# Unicorn CPU backend

Its `sys/` crate, `fujimod-unicorn-sys`, builds the pinned Unicorn source as a
static ARM archive during Cargo builds and links the archive into each
executable or extension. It has no features, so the archive is compiled once
per profile. There is no runtime Unicorn library selection. Initialize the
source submodule once in a fresh checkout:

```sh
git submodule update --init -- crates/emulator-cpu-unicorn/vendor/unicorn
```

The sys crate's build script configures CMake and builds the archive automatically.
The build requires CMake 3.24 or newer and a native C compiler. Configuration
starts with a fresh CMake cache so compiler changes cannot discard the ARM-only
selection. The build refuses a cache that selects other guest architectures.

## Memory-hook access refusal

Only the raw `Core` sets `Features::access_refusal`. A `Hook::Memory` callback calls `Cpu::refuse_access` to refuse the access it observes: later hooks, the access and its device callbacks, a requested restart and the rest of the instruction do not happen, and `run` returns `ExecutionError::AccessRefused` with the PC at the instruction. Earlier accesses of that instruction remain. Refusal requires the per-access exit checks and serial translation that `Library::open` selects, and fails with `UnsupportedExecutionMode` otherwise. The R-profile `Core` and other backends return `AccessRefusalError::Unsupported`.

The raw `Core` permits same-core nested runs from callbacks. Each run has its own memory-hook scope and refused-access error details. Returning from a nested run restores any refusal and hook scope of the suspended run. An instruction callback in a nested run cannot refuse the suspended run's memory access.

## Exact loop execution

Exact board runs use `LoopMode::Accelerate` by default. Select
`LoopMode::Measure` to count eligible instructions while executing them, or
`LoopMode::Disabled` for the ordinary backend comparison.
The board grants each exact run a stable window ending at its first device,
DMA, interrupt or runnable-peer boundary. A standalone CPU owner must grant
that window explicitly; it expires when the run returns. Measurement executes
every instruction and counts qualifying taken iterations. Acceleration charges
the same instructions and leaves partial iterations to ordinary execution.

The conservative A32 proof accepts direct backward loops of at most sixteen
instructions, unconditional integer operations, aligned RAM/declared pure
status loads, fixed architectural states and unit-step counters compared with
an invariant immediate or register. It rebuilds the proof from fetch-checked
bytes, checks data permissions, preserves NZCV, and stops before counter wrap.
Stores, atomics, system operations, conditional bodies, Thumb and unsupported
algebra continue through the CPU normally. Instruction/block observers inside
a loop, read observers, discovery, transaction recording and watched device
waits disable acceleration. Loop counters and worklist visit stamps are host
bookkeeping; pending work, deadlines and all modeled device state remain in
the machine fingerprint.
