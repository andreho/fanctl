
# Change Log
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](http://keepachangelog.com/)
and this project adheres to [Semantic Versioning](http://semver.org/).

## [1.0.0] - 2026-09-22

### Changed
- Renamed `thinkfan-tui` → `fanctl`.
- **Generalized to any `hwmon` device**, not just ThinkPads. Temperature
  reading and fan control now go through the `/sys/class/hwmon` sysfs
  interface; the `sensors` subprocess is no longer required (it remains an
  optional display source).
- Added a **per-fan YAML configuration** (`~/.config/fanctl/fanctl.yaml`),
  auto-generated on first run by probing the hardware. Each fan defines the
  temperature it follows, its control strategy, and a temperature→duty
  curve.
- Fan control is now per-fan and selectable: `curve` (fanctl writes the duty
  from the curve each tick) or `kernel-auto` (the chip's own auto-algorithm).
- **A fan's curve can now be driven by several sensors at once** (e.g. the CPU
  *and* one or more GPUs) combined with a per-fan `aggregation`
  (`max`/`avg`/`min`), instead of a single sensor.
- **NVIDIA GPU temperatures and fan RPMs** are read in-process via NVML
  (the `nvml-wrapper` crate, the same library `BDHU/gpuinfo` wraps), with a
  `nvidia-smi` CLI fallback — since the NVIDIA driver exposes no `hwmon` node
  for GPU temperature. GPUs are displayed and can be referenced as
  curve-driving sensors via `hwmon: nvidia` / `sensor: gpu<N>`.
- The TUI gained a selectable multi-fan list (arrows/`j`/`k`) with per-fan
  `a`/`f`/`o`/`0-9` control, a color-coded temperature bar view, sorting
  (`s`), and scrolling.
- Removed the ThinkPad-only `/proc/acpi/ibm/fan` backend and the
  `sudo chown` workaround (not applicable to `sysfs` nodes).

### Added
- `--probe` (list `hwmon` chips with their PWM/temp/fan sensors, **plus NVIDIA
  GPUs**) and `--summary` (one-shot text dump) CLI modes, plus `-c FILE` for an
  alternate config path.
- Unambiguous chip resolution by `hwmonN` when several chips share a name.
- Unit tests for the curve engine, the hwmon reader/writer (against a fake
  `sysfs` tree), config round-tripping, TUI rendering, and key handling.

## [0.3.1] - 2025-12-23

### Changed
- Use sudo instead of pkexec for chown
- Print full chown error if present

### Fixed
- Fix mismatched_lifetime_syntaxes warning
- Fix typos

## [0.3.0] - 2025-08-30

### Changed

- Verify that thinkpad_acpi module is loaded
- Add help popup window
- Add a shortcut (S key) to sort the inputs by temperature
- Make the Temperatures view scrollable

## [0.2.0] - 2025-06-18

### Changed

- Use colored graphs for temperature
- Exit on insufficient permissions
- Handle errors on read/write failures

## [0.1.1] - 2025-01-10

### Changed

- Cargo: pin dependency versions.

### Fixed

- Align Fan Info lines.