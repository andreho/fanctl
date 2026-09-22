# fanctl

A terminal-based Linux application for **fan control** and **temperature
monitoring** on any `hwmon`-equipped machine, driven by a per-fan YAML
configuration.

![Screenshot](screenshot.gif "Screenshot")

It works on ThinkPads, desktops, and the wide variety of Super-I/O chips
(`it8792`, `w83627hf`, `nct6775`, `lm92`, …) — anywhere the kernel exposes a
`/sys/class/hwmon` device with a PWM channel.

## How it works

- **Reads temperatures** from the kernel `hwmon` interface
  (`/sys/class/hwmon/hwmonN/temp*_input`) — CPU (coretemp/k10temp), SoCs,
  disks, thermistors, … No subprocess is forked.
- **Also reads NVIDIA GPU temperatures** (and fan RPMs) in-process via the
  NVIDIA Management Library (NVML), using the [`nvml-wrapper`] crate — the
  same approach as [BDHU/gpuinfo]. Since NVIDIA's driver does not expose a
  `hwmon` device for the GPU temperature, this avoids the `nvidia-smi`
  subprocess entirely (it remains only as a fallback if the NVML library is
  missing). GPUs show up as `nvidia:gpu<N>` sensors.
- **Optionally** also shows sensors from lm-sensors (`sensors -j`) when
  enabled in the config.
- **Controls fans** by writing to the `hwmon` PWM channels:
  - *Curve* control (default): `fanctl` evaluates a per-fan curve
    (`temperature → duty %`) and writes the result to the PWM every tick.
  - *Kernel auto*: hands the PWM back to the chip's own auto-algorithm
    (`pwm*_enable = 3`, or `4` for full speed).
- **Each fan's curve can be driven by one or several sensors** (e.g. the CPU
  *and* one or more GPUs) combined with an aggregation — see the config below.
- **Auto-generates its config** on first run by probing the hardware, then
  lets you tune it.

## The configuration file

On first run `fanctl` writes a default config to:

- `$XDG_CONFIG_HOME/fanctl/fanctl.yaml`, or
- `~/.config/fanctl/fanctl.yaml`

(You can point it elsewhere with `-c /path/to/config.yaml`.) The generated
file is a reasonable starting point — edit the curves and labels to taste.

```yaml
refresh_secs: 2.0        # how often to refresh and re-evaluate

pwm:
  # A fan driven by the hottest of several sources (CPU + two GPUs here).
  - id: pwm1
    hwmon: it8792         # chip name, or its hwmonN directory
    name: "CPU + GPU 0/1"
    pwm_max: 255          # chip's raw PWM range (see below)
    temp_sensors:         # one or more; combined with `aggregation`
      - hwmon: k10temp      # e.g. an AMD CPU temperature
        sensor: temp1
      - hwmon: nvidia       # NVIDIA GPUs are referenced by index…
        sensor: gpu0
      - hwmon: nvidia
        sensor: gpu1
    aggregation: max      # max (hottest) | avg | min
    control: curve        # curve (fanctl-driven) | kernel-auto
    curve:                # temperature→duty curve (°C → %)
      - temp: 30
        duty: 0
      - temp: 45
        duty: 40
      - temp: 60
        duty: 75
      - temp: 75
        duty: 100
    default: auto         # initial mode: auto | off | full

  # A second fan following a single GPU.
  - id: pwm2
    hwmon: it8792
    name: "GPU 2 (3080 Ti)"
    temp_sensors:
      - hwmon: nvidia
        sensor: gpu2
    aggregation: max
    control: curve
    curve:
      - temp: 30
        duty: 0
      - temp: 70
        duty: 100
    default: auto

temp_sensors:              # what the TUI displays (NVIDIA GPUs are always shown)
  - hwmon: k10temp
    sensors: [temp1, temp3]
  - hwmon: acpitz
    sensors: [temp1]

show_lm_sensors: false      # also pull lm-sensors in for display
```

The `temp_sensors` list is what a fan's curve is evaluated from; `aggregation`
combines them into the single value fed to the curve. `max` (the default) means
*“spool the fan if **any** of these is hot”** — the usual choice when mixing a
CPU with several GPUs. A single-sensor config is just a one-element list.

Notes:

- `hwmon` may be a chip name (e.g. `it8792`) or its `hwmonN` directory. If
  two chips share a name (common for two NVMe drives, both called `nvme`),
  refer to them by `hwmonN` to disambiguate.
- `pwm_max` is the chip's raw PWM maximum. Most chips don't expose it via
  sysfs, so `fanctl` defaults to `255`; if the reported duty % looks off,
  adjust this (it only affects the readback and the exact value written for a
  given percent — control stays monotonic either way).

## Controlling

In the interactive TUI:

| Key           | Action                                          |
| ------------- | ----------------------------------------------- |
| `↑` / `↓` or `j`/`k` | Select the active fan        |
| `a`           | Active fan → **Auto** (follow its curve / kernel auto) |
| `f`           | Active fan → **Full** speed                    |
| `o`           | Active fan → **Off**                          |
| `0`–`9`       | Active fan → fixed **0–90 %**                  |
| `s`           | Sort temperature rows by value                |
| `PgUp`/`PgDn`,`Home`/`End` | Scroll temperatures            |
| `?`           | Toggle the help window                         |
| `Esc`        | Close the help window                          |
| `q`           | Quit                                           |

CLI modes (no TUI):

- `fanctl --probe` — list every discovered `hwmon` chip and its PWM/temp/fan
  sensors.
- `fanctl --summary` — a one-shot text dump of fans and temperatures.
- `-c FILE` — use an alternate config file.

## Permissions

Reading temperatures (and the current PWM state) is open to every user.
**Writing** the PWM files, however, requires write access, which by default is
root-only:

```
$ ls -l /sys/class/hwmon/hwmon5/pwm1
-rw-r--r-- 1 root root … pwm1
```

Two ways to make fan control work as a normal user:

1. **Run it with `sudo`** — the simplest option:
   ```
   sudo fanctl
   ```

2. **A `udev` rule** (the same mechanism lm-sensors' `fancontrol` uses) that
   grants a group write access to the `hwmon` PWM nodes, so you can run
   `fanctl` without `sudo`:
   ```
   # /etc/udev/rules.d/90-fanctl.rules
   SUBSYSTEM=="hwmon", KERNEL=="hwmon*", MODE="0660", GROUP="cool"
   ```
   ```
   sudo groupadd cool && sudo usermod -aG cool "$USER"
   sudo udevadm control --reload-rules && sudo udevadm trigger
   # log out and back in (or: newgrp cool), then: fanctl
   ```
   Narrow the rule (e.g. match a specific `ATTR(name)=="it8792"`) if you only
   want to open one chip.

## Installation

`fanctl` is written in Rust. Install the [toolchain](https://www.rust-lang.org/tools/install), then build from this repository (the `fanctl` crate name on crates.io belongs to an unrelated project, so use `--git` rather than a bare `cargo install fanctl`):

```
cargo install --git https://github.com/andreho/fanctl
```

Pre-built binaries are available from the [releases](https://github.com/andreho/fanctl/releases) page.

## Requirements

- A Linux kernel with the `hwmon` interface and a chip that exposes a PWM
  channel. Most laptops and desktops qualify.
- `lm-sensors` is **not** required (all `hwmon` reads/writes go through sysfs),
  but it's useful if you enable `show_lm_sensors` for extra display sensors.
- **NVIDIA GPUs:** temperatures/RPMs come from NVML (`libnvidia-ml.so`,
  shipped with the NVIDIA driver), read in-process via `nvml-wrapper`. If the
  NVML library is absent, `fanctl` falls back to the `nvidia-smi` CLI; if
  neither is present, GPU sensors simply won't show.

## Troubleshooting

- **No fans shown** — run `fanctl --probe` and confirm your chip appears with
  a `pwm…` entry. Some chips expose PWMs only when a specific driver/module is
  loaded.
- **`failed to write …/pwm1_enable: Permission denied`** — you need root or the
  udev rule from above.
- **Duty % readback looks off** — set `pwm_max` on that fan to the chip's true
  PWM maximum.

## Acknowledgements

- Inspired by [thinkfan-ui](https://github.com/zocker-160/thinkfan-ui) and by
  lm-sensors' `fancontrol`.
- The NVIDIA GPU monitoring approach (in-process NVML access) follows
  [BDHU/gpuinfo](https://github.com/BDHU/gpuinfo), which uses the
  [nvml-wrapper] crate. `fanctl` depends on [nvml-wrapper] directly rather
  than the `gpuinfo` CLI, since only the NVML reads are needed.

[nvml-wrapper]: https://github.com/Cldfire/nvml-wrapper

## License

MIT. See [LICENSE](LICENSE).
