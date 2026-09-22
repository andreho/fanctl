# fanctl

A terminal-based Linux application for **fan control** and **temperature
monitoring** on any `hwmon`-equipped machine, driven by a per-fan YAML
configuration.

![Screenshot](screenshot.gif "Screenshot")

It works on ThinkPads, desktops, and the wide variety of Super-I/O chips
(`it8792`, `w83627hf`, `nct6775`, `lm92`, …) — anywhere the kernel exposes a
`/sys/class/hwmon` device with a PWM channel.

`fanctl` is a **two-part application**:

- **`fanctld`** — the **daemon**. It owns the hardware: it reads the
  temperatures, applies each fan's mode (curve, kernel auto, off, full, or a
  fixed manual duty), and reads the fans back. It runs as a service
  (e.g. under systemd) and serves clients over a Unix socket, reloading its
  config on `SIGHUP`. It also provides the one-shot `--probe` / `--summary`
  modes.
- **`fanctlui`** — the **TUI client**. It connects to `fanctld`, renders
  the fans and temperatures, and sends fan commands. It never touches the
  hardware itself, so the fans keep being driven by the daemon even when no
  one is watching the TUI.

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
- **Controls fans** by writing to the `hwmon` PWM channels, each tick:
  - *Curve* control (default): the daemon evaluates a per-fan curve
    (`temperature → duty %`) and writes the result to the PWM.
  - *Kernel auto*: hands the PWM back to the chip's own auto-algorithm
    (`pwm*_enable = 3`, or `4` for full speed).
  - *Off / Full / manual N%*: static modes settable at any time from the TUI.
- **Each fan's curve can be driven by one or several sensors** (e.g. the CPU
  *and* one or more GPUs) combined with an aggregation — see the config below.
- **The daemon and the TUI talk a small JSON protocol over a Unix socket**
  (one request line, one response line per short-lived connection; the default
  path is `$XDG_RUNTIME_DIR/fanctld.sock`, overridable with `--sock` or the
  `FANCTLD_SOCK` environment variable), so the TUI works even over SSH.

## The configuration file

On first run `fanctld` writes a default config to:

- `$XDG_CONFIG_HOME/fanctl/fanctl.yaml`, or
- `~/.config/fanctl/fanctl.yaml`

(You can point it elsewhere with `-c /path/to/config.yaml`.) The generated
file is a reasonable starting point — edit the curves and labels to taste.

```yaml
refresh_secs: 2.0        # how often the daemon refreshes and re-evaluates

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
    # Measured effective duty range, from a calibration sweep (see
    # "Calibrating a fan's duty range") — the curve is clamped to it:
    # duty_min: 20
    # duty_max: 80

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
combines them into the single value fed to the curve. `max` (the default)
means *“spool the fan if **any** of these is hot”* — the usual choice when
mixing a CPU with several GPUs. A single-sensor config is just a
one-element list.

Notes:

- `hwmon` may be a chip name (e.g. `it8792`) or its `hwmonN` directory. If
  two chips share a name (common for two NVMe drives, both called `nvme`),
  refer to them by `hwmonN` to disambiguate.
- `pwm_max` is the chip's raw PWM maximum. Most chips don't expose it via
  sysfs, so `fanctl` defaults to `255`; if the reported duty % looks off,
  adjust this (it only affects the readback and the exact value written for a
  given percent — control stays monotonic either way).
- `duty_min` / `duty_max` (optional, percent) — the measured range of duty in
  which the fan actually responds, written by a calibration sweep (see
  below). When set, the fan's *curve* output is clamped to that range, so a
  0–100 % curve maps onto the part of the duty span the fan responds to.
  Explicit duties (the TUI dialog, `--set`) are never clamped.

## Running the daemon as a service

The daemon is meant to run all the time, so the fans are always under the
daemon's control even when no TUI is attached. A ready-to-use systemd unit
lives in [`systemd/fanctld.service`](systemd/fanctld.service); to install:

```
sudo cp systemd/fanctld.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now fanctld
```

The unit runs `fanctld` as root (writing the PWM files requires it — see
[Permissions](#permissions)) and listens on `/run/fanctld.sock`. Reload the
config without restarting the daemon with:

```
sudo systemctl reload fanctld        # sends SIGHUP; the daemon re-reads fanctl.yaml
```

Or just run it by hand, wherever you like:

```
fanctld                          # default socket path
fanctld -c /path/to/config.yaml  # alternative config
fanctld --sock /path/to/s.sock   # alternative socket
```

## Controlling

In the TUI (`fanctlui`; `--sock FILE` / `FANCTLD_SOCK` to point it at a
non-default daemon socket):

| Key           | Action                                          |
| ------------- | ----------------------------------------------- |
| `↑` / `↓` or `j`/`k` | Select the active fan        |
| `a`           | Active fan → **Auto** (follow its curve / kernel auto) |
| `f`           | Active fan → **Full** speed                    |
| `o`           | Active fan → **Off**                          |
| `Enter`       | Active fan → dialog to set an exact duty (**0–100 %**) |
| `e`           | Active fan → **edit its curve and default mode** (see below) |
| `c`           | Active fan → **calibrate its duty range** (see below)        |
| `s`           | Sort temperature rows by value                |
| `PgUp`/`PgDn`, `Home`/`End` | Scroll temperatures          |
| `?`           | Toggle the help window                         |
| `Esc`        | Close the help window                          |
| `q` or `Ctrl+C` | Quit (the daemon keeps running)              |

If the daemon is not (yet) running, the TUI shows the connection error and
keeps trying on every refresh.

**Editing a fan's curve** (`e`, on the active fan) opens a table with one row
per `{temp, duty}` point and a final `default:` row:

- `↑`/`↓` move between rows (the last row is the fan's *initial mode*),
  `←`/`→` switch the cursor between the `temp` and `duty` fields — or cycle
  `auto` → `off` → `full` on the `default:` row.
- Type a number (backspace to clear); `Enter` commits the field. Duty is
  `0`–`100`, temperature `0`–`150`.
- `a` adds a point, `x` deletes the active one, `u`/`d` move it up/down.
- `s` saves: the daemon validates the points (strictly increasing
  temperatures), writes the config **atomically**, and applies the new curve
  on its next tick. A rejected save shows the reason in the dialog.
  `Esc` cancels.

**Calibrating a fan's duty range** (`c` on the active fan, or the one-shot
`fanctld --sweep FAN`) measures the range of duty in which the fan actually
responds. A 3-wire fan has no feedback wire: the fan's own electronics decide
how far a given PWM drives it. Most fans will not spin up below roughly
20–30 % duty, and many reach their top speed before 100 % — so a curve
spanning 0–100 % spends part of its range on nothing.

The sweep steps the channel's duty up from 0 % (10 % per step by default,
3 s settling after each change; `--step` / `--settle` adjust both), reads the
fan's tachometer after every step, and stops as soon as the RPM stops rising
(or at 100 %):

- In the TUI the daemon runs the sweep in the background: the TUI stays
  responsive, the fan row shows `calibrating…`, and the result appears in the
  status line when it finishes.
- `fanctld --sweep pwm1` does the same one-shot from the terminal (it refuses
  to run while a daemon is running, whose ticks would stomp the sweep),
  printing the per-step table as it goes.

When the measured range is useful (the fan spins somewhere in the middle, and
the tach actually moves with the duty), it is saved on the fan in
`fanctl.yaml` as `duty_min` / `duty_max` (atomic write) and the fan's curve
is clamped to that range — a 0–100 % curve now maps onto the part of the duty
span the fan responds to. The fan's previous mode is restored afterwards. A
channel whose tachometer never leaves 0 rpm is reported as having no
measurable fan (the header is probably empty, or the fan is driven by
something else, e.g. a GPU) — nothing is saved.

One-shot CLI modes on the daemon (no TUI needed):

- `fanctld --probe` — list every discovered `hwmon` chip and its PWM/temp/fan
  sensors, plus the NVIDIA GPUs.
- `fanctld --summary` — a one-shot text dump: the `PWM channels` section
  lists every `pwmN` channel on every chip (with raw duty, percent and mode)
  so you know which channel to address, followed by your configured fans and
  temperatures.
- `--set FAN=MODE` (both binaries) — set a fan's mode and exit, for scripts.
  `MODE` is a percent (`0`–`100`) or one of `auto|off|full`, and the flag may
  be repeated: `fanctlui --set pwm1=99 --set pwm2=off`. On `fanctlui` the
  command goes through the running daemon (a manual duty then sticks, like the
  TUI dialog); on `fanctld` it writes the chip directly and exits, so a
  running service will re-apply its own state on its next tick.
- `fanctld --sweep FAN [--step N] [--settle S]` — calibrate the fan's duty
  range (see above) and exit: prints the per-step duty/RPM table, saves a
  useful range as `duty_min`/`duty_max`, restores the fan's previous mode.
  Exit code: `0` when a usable range was found, `1` when not (or on a write
  failure), `2` when a daemon is running (it must be stopped first).
- `fanctld -c FILE` — use an alternate config.

Both binaries accept `-h` / `--help` (usage) and `-V` / `--version`;
unrecognized flags are an error and nothing is started.

## Permissions

Reading temperatures (and the current PWM state) is open to every user.
**Writing** the PWM files, however, requires write access, which by default is
root-only:

```
$ ls -l /sys/class/hwmon/hwmon5/pwm1
-rw-r--r-- 1 root root … pwm1
```

Two ways to make the daemon able to control the fans:

1. **Run the daemon as root** — what the shipped systemd unit does (the
   simplest option).
2. **A `udev` rule** (the same mechanism lm-sensors' `fancontrol` uses) that
   grants a group write access to the `hwmon` PWM nodes, so the daemon can
   then run as a normal user:
   ```
   # /etc/udev/rules.d/90-fanctl.rules
   SUBSYSTEM=="hwmon", KERNEL=="hwmon*", MODE="0660", GROUP="cool"
   ```
   ```
   sudo groupadd cool && sudo usermod -aG cool "$USER"
   sudo udevadm control --reload-rules && sudo udevadm trigger
   # log out and back in (or: newgrp cool), then run fanctld as $USER
   ```
   Narrow the rule (e.g. match a specific `ATTR(name)=="it8792"`) if you only
   want to open one chip.

## Installation

`fanctl` is written in Rust. Install the [toolchain](https://www.rust-lang.org/tools/install), then build from this repository (the `fanctl` crate name on crates.io belongs to an unrelated project, so use `--git` rather than a bare `cargo install fanctl`). Both parts (`fanctld` and `fanctlui`) are installed together:

```
cargo install --git https://github.com/andreho/fanctl
```

Pre-built binaries are available from the [releases](https://github.com/andreho/fanctl/releases) page.

## Requirements

- A Linux kernel with the `hwmon` interface and a chip that exposes a PWM
  channel. Most laptops and desktops qualify.
- `lm-sensors` is **not** required (all `hwmon` reads/writes go through
  sysfs), but it's useful if you enable `show_lm_sensors` for extra display
  sensors.
- **NVIDIA GPUs:** temperatures/RPMs come from NVML (`libnvidia-ml.so`,
  shipped with the NVIDIA driver), read in-process via `nvml-wrapper`. If the
  NVML library is absent, `fanctld` falls back to the `nvidia-smi` CLI; if
  neither is present, GPU sensors simply won't show.

## Troubleshooting

- **The TUI shows “can't reach fanctld”** — the daemon isn't running (or is
  listening on a different socket). Start it, or point the TUI at it:
  `fanctlui --sock /run/fanctld.sock`.
- **No fans shown** — run `fanctld --probe` and confirm your chip appears
  with a `pwm…` entry. Some chips expose PWMs only when a specific
  driver/module is loaded.
- **`failed to write …/pwm1_enable: Permission denied`** (shown next to a fan
  in the TUI) — the daemon needs root or the `udev` rule from above.
- **Duty % readback looks off** — set `pwm_max` on that fan to the chip's
  true PWM maximum.
- **On an it87-family chip (e.g. IT8792), switching between `off`, `full`
  and `auto` produces no audible change** — the kernel it87 driver's "off"
  mode (`pwmN_enable = 0`) does not stop the fan: it parks the fan in
  on/off mode at a fixed 100%, so off ≈ full, and the chip's own auto
  algorithm is also near 100% at high temperatures. `fanctl` therefore
  implements `off` as a 0% *manual* duty, which really stops the fan. If
  *no* duty value (0%…100%) changes the fan, that `pwmN` channel is
  probably not wired to the fan you are listening to (e.g. a GPU driving
  its own fans). Identify the wiring by toggling each channel one at a
  time: `fanctld --set pwmN=0`, then `=100`.
- **The fan ignores part of the curve (stays still at low duty, and/or
  doesn't get faster at high duty)** — run a calibration (`c` in the TUI, or
  `fanctld --sweep pwmN`): it measures where the fan actually starts and
  saturates and saves that as `duty_min`/`duty_max`, so the curve is clamped
  to the range the fan responds to.

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
