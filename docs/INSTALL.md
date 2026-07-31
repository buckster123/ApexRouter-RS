# Installing ApexRouter-RS

There are two ways in, and they are equally supported.

| | |
|---|---|
| **The guided script** — `./install.sh` | Asks (or, with `--yes`, doesn't), builds, installs, optionally wires a systemd user service, and runs `doctor` at the end. [§1](#1-the-guided-script). |
| **By hand** — this document | Every step the script takes, written out, so you can do them yourself, skip the ones you don't want, or work out which one failed. [§3](#3-prerequisites) onwards. |

Doing it by hand is not the fallback. If your instinct is *"I am not piping a script from the
internet into my shell"*, that instinct is correct and this document exists to respect it rather
than to talk you out of it. It is also the right document when an install went wrong: the script is
these commands in a loop with error handling, so the step that failed is a step below.

**Before you invest an evening in this:** read
[`docs/RELEASE-NOTES-mk1.md` §6](RELEASE-NOTES-mk1.md#6-what-to-trust--the-three-buckets). It splits
every claim into *verified*, *banked from real hardware*, and *never run against the real thing*.
The third bucket is not short, and it contains most of what a multi-GPU rig would exercise. You
knowing that going in is the entire point of a field test.

**Platform: Linux only in mk1.** The process model is `/proc`, `flock`, `setsid` and `boot_id`
(CHARTER D15). macOS and Windows are not "not tested" — they are not implemented. `Backend::Metal`
exists in the data model and nothing pretends it works.

---

## 1. The guided script

From a checkout:

```sh
git clone https://github.com/buckster123/ApexRouter-RS
cd ApexRouter-RS
./install.sh --dry-run          # print the complete plan; no writes, no network, no build
./install.sh                    # two questions, then it runs — see just below
./install.sh --yes              # unattended: every default, nothing asked
./install.sh --tui              # ask the questions even when piped; option 2 walks every choice
./install.sh --help             # every flag
```

Or, when the repository is public, the one-liner:

```sh
curl -fsSL https://github.com/buckster123/ApexRouter-RS/raw/main/install.sh | bash
curl -fsSL https://github.com/buckster123/ApexRouter-RS/raw/main/install.sh | bash -s -- --tui
```

> **Note.** The repository is not public yet. Until it is, the `curl` form will not resolve and the
> checkout form is the only one that works. Nothing in this document assumes network access to
> GitHub except where it says so.

### What a bare `./install.sh` actually asks

Two things, and no more. First, how to decide the rest:

```
    1) Automatic - use the detected answers (recommended)
    2) Manual    - walk me through every choice

  ? number [1]
```

then it prints the complete plan — every path, the build command, whether a service is installed —
and asks once more:

```
  ? [Y/n]
```

Take both defaults and that is two keypresses to an automatic install. Answer `n` at the second and
you get `Cancelled — nothing was changed.` with nothing written: the plan is printed *before*
anything is executed, always. Choose `2) Manual` at the first and every decision below becomes its
own question, with the reasoning attached.

Where `whiptail` is installed — most Debian and Ubuntu boxes — those two questions are dialog boxes
rather than the text above; the content is identical. `APEXROUTER_INSTALL_NO_WHIPTAIL=1` forces the
plain-text form, which is also what you get on a dumb terminal or under a screen reader.

**Piped, it is unattended.** `curl … | bash` has no keyboard on stdin (stdin *is* the script), so
`--yes` is implied and neither question is asked. `--tui` overrides that and reads the keyboard from
`/dev/tty` instead. `--dry-run` never asks anything, whatever else you pass it.

### The flags

Run `./install.sh --help` for the authoritative list — that is the script's own text and cannot
drift from it. The flags worth knowing before you start:

| Flag | What |
|---|---|
| `--dry-run` | Print the complete plan and touch nothing: no writes, no network, no build. This is how you audit it before running it. |
| `-y`, `--yes` | Unattended: neither question is asked. **Implied when stdin is not a terminal**, which is what a `curl \| bash` gets. |
| `--tui` | Ask the two questions even when stdin is a pipe — it reads the keyboard from `/dev/tty`, and overrides an explicit `--yes`. It is the *same* first menu; choosing **Manual** there is the step-by-step walk. |
| `--prefix DIR` | Install root for binaries. Default `$HOME/.local` → `$HOME/.local/bin`. |
| `--system` | System-wide binaries in `/usr/local/bin`. Needs root and asks for it. State, config and the service stay per-user — they hold your credentials and your ledger. |
| `--state-dir DIR` | `$APEXROUTER_HOME`. Default `$HOME/.local/state/apexrouter`. |
| `--with-gui` / `--no-gui` | Also build `apexrouter-ui`, the **GPL-3.0** Slint app. Off by default for licence reasons, not quality ones — [§10](#10-the-native-app-and-the-gpl-line). |
| `--service` / `--no-service` | The systemd `--user` unit. On by default when `systemd --user` is available. |
| `--linger` / `--no-linger` | `loginctl enable-linger $USER`, so the daemon survives a logout. An unattended run that installs the service does this by default and says so; `--no-linger` refuses. It is a per-user *system* setting — [§8](#8-running-it-as-a-service-systemd---user), and `uninstall.sh` offers to undo it. |
| `--no-completions`, `--no-modify-path` | Skip shell completions; never touch a shell rc file (you get the `export` line to paste instead). |
| `--no-rustup`, `--no-build`, `--offline` | Never install a toolchain; install an existing `target/release` build; no network at all. |
| `--from-source DIR`, `--repo-url URL`, `--branch REF` | Where the source comes from. `--from-source` means no clone, no pull, no network. |
| `--jobs N` | `cargo build` jobs. Default derived from your RAM and cores. |
| `--uninstall` | Remove binaries, unit and completions, keeping state and config. `--purge` deletes those too, and asks. |

It **never compiles llama.cpp** — that is a long, hardware-specific job and yours to choose
([§9](#9-llamacpp)). It discovers builds you already have and prints the options if you have none.
It resolves your choices into `$STATE/install.conf` and restores them on the next run, so
re-running to upgrade does not re-ask everything.

**Updating**, thereafter, is one verb: `apexrouter update` runs `git pull --ff-only` on the
checkout `install.conf` records (`--ff-only`, so a checkout you also work in is never merged for
you) and hands over to that checkout's `install.sh --yes` — same choices, same rebuild, same
final verify that the daemon serving is the binary the run wrote. `--no-pull` rebuilds what is
already checked out. An install not made by `install.sh` has nothing to update this way: its
whole update story is `git pull && cargo build --release`, as before.

Everything the script does is in this document as a command you can run yourself. If a flag you need
is missing, the manual path always works.

---

## 2. What you are installing

One binary does everything: the CLI, the daemon (`apexrouter serve`) and the MCP stdio server
(`apexrouter mcp`) are the same executable. The web UI is compiled into it — no npm, no CDN, no
build step, no second artefact.

| Thing | Default location | Made by |
|---|---|---|
| `apexrouter` | `~/.local/bin/apexrouter` | `cargo build --release` |
| `apexrouter-ui` (optional, GPL-3.0) | `~/.local/bin/apexrouter-ui` | `cargo build --release -p apexrouter-slint` |
| Shell completions | `~/.local/share/bash-completion/completions/apexrouter` (or zsh/fish equivalent) | `apexrouter completions <shell>` |
| systemd user unit | `~/.config/systemd/user/apexrouter.service` | you, in [§8](#8-running-it-as-a-service-systemd---user) |
| Config | `~/.config/apexrouter/config.toml` | optional — zero config is a working install |
| State (facts, ledger, usage, logs) | `~/.local/state/apexrouter/` | the daemon, on first run |
| Cache (HF metadata, probe results, offers) | `~/.cache/apexrouter/` | the daemon, on first run |

**Nothing is ever written into the repository directory.** That is invariant 5, and it means you
can `rm -rf` the checkout after installing and lose nothing but the source.

**Nothing is written outside your home directory**, either. No `/etc`, no `/usr`, no system service,
no `sudo` at any point in this document — unless you choose a `--prefix` that needs it.

> **One coupling worth knowing before you meet it.** The config file is
> `~/.config/apexrouter/config.toml` *only while `$APEXROUTER_HOME` is unset*. Set that variable —
> to anything, including the value that is already the default — and the config file becomes
> `$APEXROUTER_HOME/config.toml` instead. It is the second link of the chain in
> [§13](#13-configuration), it is deliberate, and it is why `install.sh` writes an
> `APEXROUTER_HOME` line into the systemd unit **only** for a non-default `--state-dir`
> ([§8](#8-running-it-as-a-service-systemd---user)).

---

## 3. Prerequisites

### 3.1 Always

| Need | Why | Check |
|---|---|---|
| **Linux** with `/proc` | process identity, adoption, `setsid` | `uname -s` → `Linux` |
| **Rust ≥ 1.75** | the workspace's `rust-version` | `rustc --version` |
| **A C linker** (`cc`, `binutils`) | Rust links with the system linker | `cc --version` |
| **git** | to get the source | `git --version` |
| ~**600 MB** disk for a release-only `target/` | build artefacts. A full debug + test build is several times larger — budget 10 GB if you intend to run the suite | `df -h .` |

There is **no OpenSSL dependency**. TLS is `rustls`, so you do not need `libssl-dev`,
`pkg-config`, or a working `openssl` at build time. This is deliberate: it is the single most common
reason a Rust build fails on someone else's machine.

Per distro, the whole build dependency list:

```sh
# Debian / Ubuntu
sudo apt install build-essential git curl

# Fedora / RHEL
sudo dnf install @development-tools git curl

# Arch
sudo pacman -S base-devel git curl

# openSUSE
sudo zypper install -t pattern devel_basis && sudo zypper install git curl
```

Rust itself, if you don't have it:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
```

A distro `rustc` is fine too if it is ≥ 1.75. Debian bookworm's 1.63 is not; `rustup` is the path of
least resistance there.

### 3.2 Optional, and what you lose without it

| Need | Unlocks | Without it |
|---|---|---|
| **llama.cpp** (`llama-server`) | local model serving — the main event | ApexRouter still proxies to remote and managed backends; `apexrouter rig` shows no builds. See [§9](#9-llamacpp). |
| **`ssh`** (OpenSSH client) | supervised `ssh -L` tunnels to rented boxes | `apexrouter tunnel` and the whole vast.ai path are unavailable. `doctor` reports `ssh.binary`. |
| **`xdg-open`** | `apexrouter open` launching your browser | open `http://127.0.0.1:2739` yourself. |
| **A GPU driver stack** (Vulkan / CUDA / ROCm) | offloading layers | CPU inference, which works and is slow. |
| **fontconfig + a display stack** | building the native Slint app | the headless stack is unaffected; the web UI is the GUI. |

### 3.3 Accounts you do *not* need

None. Zero credentials is a working install. vast.ai, together.ai and HuggingFace are each optional
and each is read from where it already lives if you happen to have it — see [§11](#11-credentials).

---

## 4. Get the source

```sh
git clone https://github.com/buckster123/ApexRouter-RS
cd ApexRouter-RS
git checkout v0.1.0-mk1          # the tag these notes describe
```

If you were handed a tarball or a local checkout instead, everything below works identically — no
step in this document contacts GitHub. Verify you're in the right place:

```sh
test -f Cargo.toml && grep -q apexrouter-protocol Cargo.toml && echo ok
```

---

## 5. Build

```sh
export CARGO_BUILD_JOBS=4         # optional; lower it further on a small machine
cargo build --release
```

`lto = "thin"` and `codegen-units = 1` buy a faster binary with a slower link. Measured here, into
an empty target directory with the cargo registry already populated: **2m01s at `-j4`** on a
12-core laptop. Add the registry download the first time you ever build a Rust project. It produces
exactly one binary you care about:

```
target/release/apexrouter          # 18,950,104 bytes — ~19 MB, stripped, LTO'd
```

`target/release/` ends up at **552 MB** including intermediate artefacts (measured, same build).
You can delete the whole `target/` directory once the binary is installed.

### What a plain `cargo build` does and does not include

The workspace has nine members but only seven are `default-members`. A bare `cargo build`,
`cargo test`, `cargo clippy` at the workspace root touches **only** the permissive headless stack:

```
apexrouter-protocol  apexrouter-core  apexrouter-router  apexrouter-providers
apexrouter-client    apexrouter-server  apexrouter-cli
```

`apexrouter-slint` (GPL-3.0-only) and `tests-support` are members but **not** default-members, so
the ordinary build never compiles or links anything GPL. This is enforced by the workspace layout
rather than by discipline — see [`docs/LICENSING.md`](LICENSING.md).

### Optional: prove it to yourself before installing

```sh
cargo test --workspace --exclude apexrouter-slint     # 1,608 tests
```

The suite is **hermetic**: no test connects anywhere but `127.0.0.x`, and that is itself guarded by
a test. It does not need a GPU, a model, or any credential. If it passes on your box, the parts of
the system that can be tested without your hardware work on your box.

### Optional: install with cargo instead

```sh
cargo install --path crates/apexrouter-cli --locked
```

This puts `apexrouter` in `~/.cargo/bin` instead of `~/.local/bin`. `--locked` uses the checked-in
`Cargo.lock`, which is what CI builds against. If you use this, substitute `~/.cargo/bin` for
`~/.local/bin` everywhere below.

---

## 6. Install the binary

```sh
mkdir -p ~/.local/bin
install -m 0755 target/release/apexrouter ~/.local/bin/apexrouter
```

Make sure `~/.local/bin` is on your `PATH` — on most distros it already is if the directory existed
at login. Appending to `~/.profile` does **not** change the shell you are standing in, and the next
command in this section needs `apexrouter` to be runnable *now*, so do both:

```sh
if ! command -v apexrouter >/dev/null; then
  echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.profile   # for every future login shell
  export PATH="$HOME/.local/bin:$PATH"                        # and for this one
fi
```

Shell completions, optional but pleasant given the verb count:

```sh
# bash
mkdir -p ~/.local/share/bash-completion/completions
apexrouter completions bash > ~/.local/share/bash-completion/completions/apexrouter

# zsh  (ensure the dir is in $fpath)
mkdir -p ~/.local/share/zsh/site-functions
apexrouter completions zsh > ~/.local/share/zsh/site-functions/_apexrouter

# fish
mkdir -p ~/.config/fish/completions
apexrouter completions fish > ~/.config/fish/completions/apexrouter.fish
```

Confirm:

```sh
apexrouter version
# apexrouter 0.1.0
# daemon not running
```

---

## 7. First run

Four commands, in this order. None of them starts a daemon, spends money, or loads a model. Each is
safe to run on a machine you have never run this on.

```sh
apexrouter doctor      # the check registry: what's here, what's missing, one fix line per row
apexrouter rig         # GPUs (free/total), llama.cpp builds, RAM, swap
apexrouter models ls   # local GGUFs, multi-shard models grouped into one row
apexrouter fit <model> --ctx 32768    # what fits on this rig, and the reasoning behind the verdict
```

`doctor` is the one to read carefully. Every row that is not `pass` carries its own **fix line**, and
the fix lines are printed together after the table. This is a real run on this project's dev box,
with only the home directory shortened:

```
(offline — apexrouterd is not running)
CHECK                STATUS   MS   DETAIL
creds.vast           pass     0    present, from /home/you/.config/vastai/vast_api_key
creds.hf             pass     0    present, from /home/you/.cache/huggingface/token
creds.together       pass     0    present, from /home/you/.vastai-gguf/config.toml
ports.proxy          warn     0    127.0.0.1:8888 is already bound — by our daemon, or by something else
ports.control        warn     0    127.0.0.1:2739 is already bound — by our daemon, or by something else
builds.discovered    pass     0    5 build(s): build, build-mtp, build-rocm, build-vulkan, build-zaya1
builds.flags         warn     0    no flags read from: build-rocm
devices.enumerated   pass     0    2 device(s), 0 software rasteriser(s) ignored
models.discovered    pass     2    1 model(s), 6.9 GiB on disk
state.writable       pass     0    /home/you/.local/state/apexrouter is writable
legacy.migration     warn     0    legacy state present: /home/you/.vastai-gguf, /home/you/…/LocalRouter
ssh.binary           pass     4    OpenSSH_10.2p1 Ubuntu-2ubuntu3.5, OpenSSL 3.5.5 27 Jan 2026
ssh.controlmaster    skipped  0    no instance selected — nothing to multiplex to
vast.credit          pass     627  $7.73 credit
vast.orphans         pass     517  the ledger and the fleet agree
together.ratelimits  pass     415  not rate limited; the provider publishes no rate-limit headers
proxy.roundtrip      skipped  0    no daemon is running — start one with `apexrouter serve`
endpoint.orphans     pass     0    0 endpoint record(s), all accounted for
net.stall            skipped  0    no instance selected — pass one, or rent one first
  fix ports.proxy: `apexrouter status` says whether it is ours; otherwise stop endpoint_proxy.py
  fix ports.control: `apexrouter status` says whether it is ours; otherwise stop endpoint_proxy.py
  fix builds.flags: run `<build>/bin/llama-server --help` by hand — a RUNPATH problem shows up here
  fix legacy.migration: see what it would do: `apexrouter migrate --dry-run`

12 pass · 4 warn · 0 fail · 3 skipped — nothing is broken
```

The rows and their order come from a static check registry, so you get **the same table**; the
statuses are what differ. On a first run with nothing else listening, the two `ports.*` rows say
`is free` and pass — they warn above only because a daemon was already up when this ran. The
`creds.*` rows skip when you have no account with that provider, which is fine: zero credentials is
a working install.

Warnings are normal on day one, and `(offline — …)` is not a problem: `doctor`, `status`, `rig`,
`models ls` and `usage` all answer from `$STATE` with the daemon down, and tag the answer
`served_by: "offline"` so a script can tell. A `builds.discovered` warning means
[§9](#9-llamacpp) is your next stop.

Every check can be run alone — the argument is an exact id, a namespace, or any fragment:

```sh
apexrouter doctor --only creds
apexrouter doctor --only ports
apexrouter doctor --json | jq
```

The check ids, so you can name them in a bug report: `state.writable`, `ports.proxy`,
`ports.control`, `creds.vast`, `creds.hf`, `creds.together`, `builds.discovered`, `builds.flags`,
`devices.enumerated`, `models.discovered`, `legacy.migration`, `endpoint.orphans`,
`proxy.roundtrip`, `ssh.binary`, `ssh.controlmaster`, `together.ratelimits`, `vast.credit`,
`vast.orphans`, `net.stall`.

### Start the daemon and serve something

```sh
apexrouter serve --detach                    # or --foreground, or let a verb autostart it
apexrouter up <model-name> --alias auto      # solves the fit, spawns llama-server, health-gates,
                                             # binds the alias, prints the base URL
apexrouter status                            # what's bound, what's live, what it's doing
```

`apexrouter up` accepts a recipe id, a model id, a unique model-name prefix, or a path on disk,
resolved in that order. `--ctx`, `--parallel`, `--devices` and `--mode` override the solver;
`--force` starts anyway when the solver says it won't fit.

Then point something at it — [§12](#12-point-a-client-at-it).

---

## 8. Running it as a service (systemd `--user`)

Optional. The daemon is perfectly happy started by hand or autostarted by any mutating verb. A
service is for "I want it up after a reboot without thinking about it".

This is what `install.sh` writes for a default install, so doing it by hand and doing it with the
script leave you in the same place. Write `~/.config/systemd/user/apexrouter.service`:

```ini
[Unit]
Description=ApexRouter-RS — one base URL for every model you can reach
Documentation=https://github.com/buckster123/ApexRouter-RS
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Environment=RUST_LOG=info
ExecStart=%h/.local/bin/apexrouter serve --foreground
Restart=on-failure
RestartSec=3

# THIS LINE IS LOAD-BEARING — see below.
KillMode=process
KillSignal=SIGTERM
# [server] drain_timeout_secs defaults to 30 — in-flight requests get to finish.
TimeoutStopSec=45

[Install]
WantedBy=default.target
```

### There is deliberately no `Environment=APEXROUTER_HOME=` line

Do not add one "for clarity". `$APEXROUTER_HOME` does two jobs, and the second one is easy to miss:
it selects the state directory *and* it becomes the second link of the config chain, so
`core/src/paths.rs` reads the config from `$APEXROUTER_HOME/config.toml` whenever the variable is
set **at all** — including when it is set to the value that was already the default. Watch it move:

```sh
$ apexrouter config path
/home/you/.config/apexrouter/config.toml

$ APEXROUTER_HOME=$HOME/.local/state/apexrouter apexrouter config path
/home/you/.local/state/apexrouter/config.toml
```

Put that line in the unit and the daemon silently stops reading
`~/.config/apexrouter/config.toml` — the path [§2](#2-what-you-are-installing),
[§13](#13-configuration) and the README all tell you to use. No error, no warning; every setting you
wrote there just has no effect. `install.sh` omits the line for exactly this reason.

**If your state lives somewhere else**, that is when the line is correct and necessary — and then
the config moves with it, by design:

```ini
# ONLY for a non-default state dir. Your config is then <that dir>/config.toml.
Environment=APEXROUTER_HOME=/data/apexrouter
```

`install.sh` applies precisely that rule: it writes the line when, and only when, `--state-dir` is
not the default. If you would rather keep the config where it is while moving the state, set
`Environment=APEXROUTER_CONFIG=%h/.config/apexrouter/config.toml` as well — that is the first link
of the chain and it wins.

Then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now apexrouter
systemctl --user status apexrouter
journalctl --user -u apexrouter -f           # the daemon logs to stderr; journald gets all of it
```

There is no `ExecStop`. systemd sends `SIGTERM` to the main process, the daemon catches it and
drains in-flight requests within `drain_timeout_secs`, and `TimeoutStopSec=45` gives it room to.
Adding `ExecStop=apexrouter serve --stop` would have a second thing racing the first.

### `KillMode=process` is not optional, and here is why

ApexRouter spawns `llama-server` children with `setsid()` so that they **outlive the manager**.
Restart the daemon, upgrade the binary, crash it — the model that took 90 seconds and 6 GB to load
is still resident, and gets re-adopted by `(pid, start_time_ticks, boot_id, exe)` on the next start.
That is one of the better properties of the whole system.

systemd's default `KillMode=control-group` throws it away. `setsid()` creates a new *session*, not a
new *cgroup*, so on `systemctl --user restart apexrouter` systemd would `SIGTERM` every process in
the unit's cgroup — including the model you just spent 90 seconds loading. `KillMode=process` stops
only the main process, which is exactly the semantics the supervisor was built for.

If you *want* children to die with the daemon, do it in config rather than in systemd, so
ApexRouter knows:

```toml
[supervisor]
kill_children_on_exit = true
```

**vast.ai instances are never destroyed on shutdown at any setting.** A crash must not delete a box
you are paying for (invariant 4).

### Surviving logout

A `--user` unit stops when your last session ends unless lingering is on:

```sh
loginctl enable-linger "$USER"      # undo: loginctl disable-linger "$USER"
```

This is a per-user *system* setting, not a file in your home directory, and it is the one thing
either script changes that an `rm -rf` would not undo. `install.sh` turns it on for you when an
unattended run installs the service — it announces it, and `--no-linger` refuses. `uninstall.sh`
reports it and offers to turn it back off ([§15](#15-uninstalling)).

### Verify

```sh
curl -s 127.0.0.1:2739/health          # control plane
curl -s 127.0.0.1:8888/v1/models       # proxy — the aggregated model list
apexrouter status
```

---

## 9. llama.cpp

ApexRouter does not bundle, vendor, link or build llama.cpp. It **spawns `llama-server` as a
separate process** and supervises it, which is why llama.cpp's licence does not propagate here
([`docs/LICENSING.md`](LICENSING.md)). You bring your own build. Three ways.

### 9.1 You already have one

Point `build_roots` at it. Discovery globs `build*/bin/llama-server` under every configured root
**and** looks for a bare `llama-server` on `$PATH`:

```toml
[endpoints]
build_roots = ["~/llama.cpp", "~/Projects/llama.cpp", "/opt/llama.cpp", "/usr/local/bin"]
```

```sh
apexrouter rig        # every build found, its backend, its flag support, its version
```

Multiple builds are supported and normal — one Vulkan, one CUDA, one CPU. ApexRouter labels each by
its build-directory name and picks per launch.

### 9.2 A distro or package-manager build

```sh
# Arch
sudo pacman -S llama.cpp                     # or llama.cpp-vulkan / llama.cpp-cuda from the AUR
# Homebrew on Linux
brew install llama.cpp
# a release binary from github.com/ggml-org/llama.cpp/releases, unpacked anywhere
```

Then make sure the directory holding `llama-server` is on `$PATH` or in `build_roots`.

### 9.3 Build your own

```sh
git clone https://github.com/ggml-org/llama.cpp ~/llama.cpp
cd ~/llama.cpp
cmake -B build-<backend> <FLAGS> -DCMAKE_BUILD_TYPE=Release
cmake --build build-<backend> --config Release -j"$(nproc)"
```

The backend matrix:

| Your hardware | `<FLAGS>` | You also need | Notes |
|---|---|---|---|
| **NVIDIA** | `-DGGML_CUDA=ON` | CUDA toolkit ≥ 12, `nvcc` on `PATH` | The best-supported path in llama.cpp. |
| **AMD, ROCm-supported card** | `-DGGML_HIP=ON -DAMDGPU_TARGETS=gfx1100` | ROCm ≥ 6, `hipcc` | Set `AMDGPU_TARGETS` to *your* arch — `rocminfo \| grep gfx`. A wrong arch either fails to link or produces silent garbage. |
| **Anything with a Vulkan driver** (AMD, Intel, NVIDIA, iGPUs) | `-DGGML_VULKAN=ON` | `libvulkan` + headers + `glslc` (`shaderc`) | The most portable GPU path, and the one this project develops on. Works on iGPUs where ROCm refuses to. |
| **Intel Arc / oneAPI** | `-DGGML_SYCL=ON` | oneAPI base toolkit | Never exercised by this project. |
| **Apple Silicon** | Metal is on by default | — | Moot: ApexRouter mk1 is Linux-only (D15). |
| **CPU only** | *(no flag)* | — | Works. Slow. Perfectly reasonable for a 3B. |

Two things that have cost real time here, both filed as sharp edges:

- **A build directory's name tells you nothing about what it can do.** On this project's dev box,
  `~/llama.cpp/build` is a *working ROCm* build and `~/llama.cpp/build-rocm` is *broken*. ApexRouter
  therefore never infers capability from a name: it runs `llama-server --list-devices` and falls
  back to inspecting sibling `libggml-*.so`. (Grepping `--help` was tried and measured wrong — it
  reported `cuda` on an AMD box.) Name your build dirs whatever you like; `apexrouter rig` will tell
  you the truth about each one.
- **A Vulkan build's trailing-colon `RUNPATH` will happily load a sibling build's `.so`.** If you
  have several build dirs next to each other, `build-vulkan/bin/llama-server` can end up running
  `build-cuda`'s `libggml-*.so`. ApexRouter sets `LD_LIBRARY_PATH=dirname(binary)` explicitly on
  every child and on every probe. **If you run `llama-server` by hand, do the same**, or you will
  debug a phantom.

### 9.4 Where GGUFs live

```toml
[endpoints]
model_roots = ["~/models", "~/.cache/huggingface/hub"]
```

Searched recursively. Per-model subdirectories and multi-shard models (`-00001-of-000NN`) are
grouped into one logical row. `apexrouter models ls` shows what it found;
`apexrouter hf search` / `apexrouter hf get` will fetch more.

---

## 10. The native app, and the GPL line

The GUI you get for free is the **web UI**, served from the control port by the same binary. Three
first-party files, no vendored JavaScript, no CDN, no build step:

```sh
apexrouter open                     # or just visit http://127.0.0.1:2739
```

The **native app** is separate and separately licensed:

```sh
cargo build --release -p apexrouter-slint
install -m 0755 target/release/apexrouter-ui ~/.local/bin/apexrouter-ui
```

`apexrouter-slint` links the [Slint](https://slint.dev) toolkit under Slint's GPL option, so that
binary is **GPL-3.0-only**. It is kept out of `default-members` precisely so an ordinary build never
pulls it in, and so an installer's default never hands you GPL obligations you did not ask for.
It is an *edge client*: it depends only on `apexrouter-protocol` and `apexrouter-client` and talks
the same HTTP/WebSocket API as everything else, so no GPL code flows back into the daemon.
Full detail: [`docs/LICENSING.md`](LICENSING.md).

All three licence texts are at the repository root and you can read them before you build anything:
[`LICENSE-MIT`](../LICENSE-MIT) and [`LICENSE-APACHE`](../LICENSE-APACHE) for the headless stack, at
your option, and [`LICENSE-GPL`](../LICENSE-GPL) for `crates/apexrouter-slint` alone. The last two
are byte-for-byte the upstream texts from `apache.org` and `gnu.org`.

---

## 11. Credentials

**Credentials are borrowed, never copied.** A key that already lives in your vast.ai, HuggingFace or
together.ai config stays exactly where it is; ApexRouter records *where it is*, not what it says.

| Provider | Read from | Notes |
|---|---|---|
| **vast.ai** | `~/.config/vastai/vast_api_key` | the `vastai` CLI's own path. Read, never written. |
| **HuggingFace** | `~/.cache/huggingface/token` | `huggingface_hub`'s own path. Read, never written. |
| **together.ai** | `$TOGETHER_API_KEY`, or `[providers.together] api_key_env` | any OpenAI-compatible provider works the same way. |

Only a key you explicitly type into a GUI or pipe to `--key-stdin` is written, and then to
`$STATE/credentials.toml` at mode `0600`. The API returns `{source: "env:TOGETHER_API_KEY",
present: true}` — the source, never the value. `Secret` prints `***`. No secret ever reaches an
argv, a query-string span, or a vast `--onstart-cmd`.

```sh
apexrouter doctor --only creds      # what was found, and where — answers offline
apexrouter provider ls              # configured providers and their key sources
```

`provider ls` is the one of the two that needs the daemon: it is classed `Mutate`, so it **starts
`apexrouterd` if it is not already running**, and it fails rather than answering if the daemon
cannot come up (a busy port, most often — `apexrouter doctor --only ports`). `doctor --only creds`
reads `$STATE` and the credential paths directly and never starts anything.

Adding another OpenAI-compatible provider is three lines of config:

```toml
[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
```

**Money.** vast.ai rentals require a `SpendApproval`, the ledger row is written *before* the billing
call, and nothing that costs money is auto-destroyed — not on shutdown, not on crash, at no setting.
There is a hard daemon-side ceiling you should set before you rent anything:

```toml
[vast]
max_usd_per_hour_ceiling = 4.0      # a SpendApproval cannot exceed this
require_human_confirm = false       # true ⇒ an approval from an agent waits for a human
```

---

## 12. Point a client at it

The whole product is that this URL never changes again.

```sh
eval "$(apexrouter env)"
# export OPENAI_BASE_URL=http://127.0.0.1:8888/v1
# export OPENAI_API_KEY=not-needed
# export ANTHROPIC_BASE_URL=http://127.0.0.1:8888
```

Both `http://127.0.0.1:8888` and `http://127.0.0.1:8888/v1` work — a missing `/v1` is added and a
repeated one collapsed, so an SDK that appends `/v1` for you and a script that already has it both
land in the same place.

```sh
curl -s http://127.0.0.1:8888/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"hi"}]}'
```

`auto` is an alias, not a model. Move it and nothing downstream notices:

```sh
apexrouter swap auto --to <model|recipe|backend-id>
apexrouter switch together
```

| Client | Wiring |
|---|---|
| **OpenAI SDK** (any language) | `base_url = "http://127.0.0.1:8888/v1"`, any non-empty api key |
| **Claude Code** | `ANTHROPIC_BASE_URL=http://127.0.0.1:8888` — the Anthropic ingress translates both ways |
| **MCP** | `claude mcp add apexrouter -- ~/.local/bin/apexrouter mcp` — 24 tools, works with the daemon down |
| **Agent Skills** | `cp -r skills/apexrouter ~/.claude/skills/` |
| **Web UI** | `http://127.0.0.1:2739` |
| **A thin edge box** | `apexrouter mcp --proxy http://fat-node:2739`, so only one machine holds credentials |

Copy-paste registration per harness is in [`docs/AGENTS.md`](AGENTS.md). Routing tables, strategies
and failover are in [`docs/ROUTING.md`](ROUTING.md).

### Serving a LAN, not just this box

Both listeners are loopback by default and a non-loopback bind **refuses to start** without a token:

```sh
apexrouter token create                                 # shown once, never stored by this command
export APEXROUTER_TOKEN=<the token>
apexrouter serve --allow-remote --control-bind 0.0.0.0:2739
```

Being reached by a real LAN peer, and the mutation gate's behaviour against a real browser, are in
the *unverified* bucket ([release notes §6.3](RELEASE-NOTES-mk1.md#63-waiting-on-real-hardware--the-honest-list)).
If you try it, that is exactly the kind of report worth filing.

---

## 13. Configuration

Zero config is a working install. Every field has a default, and every value in
[`config.example.toml`](../config.example.toml) **is** that default — deleting a line changes
nothing.

```sh
apexrouter config path              # where it resolves to
apexrouter config init              # write the fully commented example there
apexrouter config show              # the effective configuration
apexrouter config show --json | jq .unknown_keys
apexrouter config edit              # in $VISUAL / $EDITOR
```

Resolution order: `$APEXROUTER_CONFIG` → `$APEXROUTER_HOME/config.toml` →
`$XDG_CONFIG_HOME/apexrouter/config.toml` → `~/.config/apexrouter/config.toml`.

**Mind the second link.** `$APEXROUTER_HOME` moves the config as well as the state, the moment it is
set to anything at all — so if `apexrouter config path` is not printing what you expect, the answer
is almost always that something in your environment or your systemd unit is exporting it. Run
`apexrouter config path` rather than assuming; it is the resolver's own answer, not a guess. This is
the trap [§8](#8-running-it-as-a-service-systemd---user) is built to keep you out of.

An unknown key is **not** an error — an older binary has to survive a newer file — but it is not
ignored in silence either: it warns on stderr naming the key it was probably meant to be, and shows
up under `unknown_keys` in `config show`. Your file is never rewritten.

The keys that actually matter on day one:

| Key | Default | Why you'd change it |
|---|---|---|
| `[endpoints] build_roots` | `["~/llama.cpp", "~/Projects/llama.cpp", "/usr/local/bin"]` | your llama.cpp lives somewhere else |
| `[endpoints] model_roots` | `["~/models", "~/.cache/huggingface/hub"]` | your GGUFs live somewhere else |
| `[endpoints] vram_margin_mb` | `1024` | headroom held back from every VRAM budget. Raise it if something else shares the GPU |
| `[endpoints] port_range` | `[8100, 8199]` | local `llama-server` ports collide with something |
| `[server] proxy_bind` | `127.0.0.1:8888` | port already taken. `$PROXY_PORT` also overrides it |
| `[server] control_bind` | `127.0.0.1:2739` | same. Clients discover it from the lock file, so you rarely type it |
| `[router] default_alias` | `"auto"` | you want a different name for "whatever is current" |
| `[router] unknown_model` | `"reject"` | `"fallback"` sends an unknown model name to the default alias instead of `404`ing. Reject is the default deliberately: a fat-fingered model name should not silently bill a rented H100 |
| `[router] anthropic_tools` | `true` | set `false` and a `/v1/messages` body carrying tools is refused *loudly*, naming the key |
| `[router] capture_bodies` | `false` | prompts and completions are never stored unless you turn this on |
| `[supervisor] kill_children_on_exit` | `false` | `true` if you'd rather models die with the daemon (see [§8](#8-running-it-as-a-service-systemd---user)) |
| `[supervisor] health_deadline_ms` | `600000` | how long a launch may make **no progress** — not how long it may take |
| `[router] warm_queue_max` | `32` | how many requests may park on an alias during a sequential swap before it starts refusing |
| `[vast] max_usd_per_hour_ceiling` | `4.0` | the hard cap no approval can exceed |

Full reference with a comment on every field: [`config.example.toml`](../config.example.toml).
Route definitions: [`routes.example.toml`](../routes.example.toml) and
[`docs/ROUTING.md`](ROUTING.md).

---

## 14. Coming from LocalRouter

```sh
apexrouter migrate --dry-run        # prints the whole plan and writes nothing. This is the default.
apexrouter migrate --apply
```

It reads `~/.vastai-gguf` and a LocalRouter checkout **read-only** and never writes to either.
Credentials are imported as a *reference* (`api_key_env` / `api_key_file`), never copied. Stale
state is treated as the normal case rather than as an error. `[compat] mirror_usage_log` defaults
**off** — an acceptance run once appended 15 rows to a real `usage.log` and they had to be restored.

Full procedure, including how to fingerprint the legacy tree before and after:
[`docs/MIGRATION.md`](MIGRATION.md).

---

## 15. Uninstalling

```sh
./uninstall.sh                 # binaries, completions, systemd unit. Keeps your data.
./uninstall.sh --dry-run       # print the plan, touch nothing
./uninstall.sh --purge         # …and delete config, state and cache too
./uninstall.sh --disable-linger  # also undo `loginctl enable-linger`
./uninstall.sh --help          # every flag
```

`./install.sh --uninstall` does the same job if that is the script you still have to hand: with a
checkout present it hands over to `uninstall.sh` (`--prefix`, `--purge`, `--dry-run` and `--yes` are
passed through), and only falls back to its own smaller routine when there is no checkout — which is
what `curl … | bash` followed by `rm -rf` leaves you with. It does **not** pass a `--state-dir`
through, so for a non-default state dir call `uninstall.sh` directly, or export `$APEXROUTER_HOME`
first.

It prints exactly what it will delete before deleting anything, and defaults to **keeping** your
config, state, ledger, usage history and `install.conf` — so a later reinstall picks your choices
back up. It never touches `~/.vastai-gguf/`, `~/models`, `~/llama.cpp`, or your HuggingFace and
vast.ai credential paths, and it will refuse if you point it at one of them by accident. It does not
edit shell rc files either: if `install.sh` added a marked `PATH` line, you are told where it is
rather than having it rewritten under you.

It resolves your paths the way the binary does, which matters if your state is not in the default
place: `--state-dir` if you pass it, then `$APEXROUTER_HOME`, then an
`Environment=APEXROUTER_HOME=` line in the systemd unit it is about to remove, then the default. And
because the config *follows* the state dir (§8), an install with its own `--state-dir` has no
`~/.config/apexrouter` — so `--purge` will not offer to delete that directory, which would belong to
a different install.

```sh
./uninstall.sh --prefix /usr/local --state-dir /data/apexrouter --dry-run
```

Run `--dry-run` first whenever your install was not the stock one: the plan **is** the list of paths
it resolved, so it is also how you check that it found the right install before it removes anything.

Three things it warns about before it does anything, because afterwards you cannot see them:

- **`llama-server` children outlive the manager by design.** Removing ApexRouter does not free their
  VRAM. Stop them first with `apexrouter endpoint stop <id>`.
- **Live vast.ai instances keep billing.** Nothing that costs money is ever auto-destroyed — that is
  invariant 4 and it is a feature right up until you forget a box exists. Check
  `apexrouter vast ls` *before* you remove the binary that can list it. It reads the ledger, so it
  answers with the daemon down.
- **`loginctl enable-linger` is not a file.** If a service was installed, linger was probably turned
  on ([§8](#8-running-it-as-a-service-systemd---user)) — a per-user system setting that no amount of
  deleting undoes. The script says so, and asks whether to put it back; `--disable-linger` and
  `--keep-linger` answer that question up front. Left alone by default, because it is not ours to
  change silently and you may have wanted it for something else.

By hand:

```sh
rm -f  ~/.local/bin/apexrouter ~/.local/bin/apexrouter-ui
rm -f  ~/.local/share/bash-completion/completions/apexrouter
rm -f  ~/.local/share/zsh/site-functions/_apexrouter
rm -f  ~/.config/fish/completions/apexrouter.fish
systemctl --user disable --now apexrouter        # `;` not `&&` — it fails if never enabled
rm -f  ~/.config/systemd/user/apexrouter.service
systemctl --user daemon-reload
loginctl disable-linger "$USER"                  # only if you turned it on for this
# and, only if you mean it:
rm -rf ~/.local/state/apexrouter ~/.cache/apexrouter
rm -rf ~/.config/apexrouter                      # NOT if you set $APEXROUTER_HOME — see §8
```

---

## 16. Troubleshooting

These are the edges this project has actually cut itself on. Every one of them is here because it
cost someone hours.

### Build

**`error: package requires rustc 1.75 or newer`** — `rustup update stable`, or install rustup if you
are on a distro toolchain older than that. Debian bookworm ships 1.63.

**The build gets OOM-killed or thrashes swap.** `cargo` defaults to one codegen job per core and the
release profile uses `lto = "thin"` with `codegen-units = 1`. Lower it:
`export CARGO_BUILD_JOBS=2`. On a 16 GB box with 12 cores this is the difference between a build and
a reboot.

**`cargo build -p apexrouter-slint` fails and `cargo build` did not.** Expected — the Slint app is
not in `default-members` and needs a font and windowing stack (`fontconfig`, `libxkbcommon`, and
either X11 or Wayland dev packages). The daemon, proxy, CLI, MCP server and web UI do not need any
of it. If you do not want the native GUI, you never need to fix this.

**Something wants `libssl-dev` / `pkg-config` / OpenSSL.** Nothing in this workspace does — TLS is
`rustls`. If you see this, a *different* crate in your environment is being built; check you are in
the right directory.

### Discovery and GPUs

**`apexrouter rig` finds no llama.cpp build.** Discovery globs `build*/bin/llama-server` under
`[endpoints] build_roots` and looks for a bare `llama-server` on `$PATH`. A build in
`~/src/llama/out/bin/` matches neither — add its root to `build_roots`, or symlink the binary
somewhere on `$PATH`.

**`apexrouter rig` reports the wrong backend for a build.** It shouldn't: detection is
`llama-server --list-devices`, with sibling `libggml-*.so` inspection as fallback. Grepping `--help`
was tried, and measured wrong — it reported `cuda` on an AMD box. If `--list-devices` disagrees with
reality on your rig, that is a *genuinely interesting bug report*; include the raw
`--list-devices` output.

**Do not trust a build directory's name, and neither does ApexRouter.** On the dev box here,
`~/llama.cpp/build` is a working ROCm build and `~/llama.cpp/build-rocm` is broken. If you have
several builds, ask `apexrouter rig` which is which rather than inferring.

**`builds.flags warn — no flags read from: <build>`.** ApexRouter found a `llama-server` there but
could not get `--help` out of it, so it does not know which flags that build supports. Usually the
build is broken or its libraries do not resolve. The fix line says it: run
`<build>/bin/llama-server --help` by hand — a RUNPATH problem shows up immediately, and so does a
build that never finished linking. A single warned-about build does not stop the working ones from
being used.

**A build seems to load the wrong GPU library.** `build-vulkan`'s trailing-colon `RUNPATH` picks up
a sibling build's `.so`. ApexRouter sets `LD_LIBRARY_PATH=dirname(binary)` on every child and probe;
if you are reproducing something by running `llama-server` yourself, set it too.

**A `WARN` about a device reporting more free memory than it has.** Expected on ROCm, and a
deliberate piece of honesty rather than a fault. You will see, on stderr:

```
WARN device reports more free memory than it has (GTT/shared-memory accounting); clamped to total
     device="ROCm0" build="build" reported_free_mb=17708 total_mb=11397
```

**ROCm reports free > total**, so `total - free` is a nonsense number and ApexRouter never computes
it. `--list-devices` is the single point at which device memory enters the program, and its one
constructor enforces `free <= total`. The clamp is logged rather than silent because the raw
reading is a fact about your driver you may want to know.

**`fit` proposes something absurd.** `apexrouter fit <model> --ctx N` prints its reasoning, not just
its verdict — read it before overriding. `apexrouter up --force` starts anyway. If the *inputs* look
wrong (`apexrouter rig` showing impossible memory after the clamp above), that is a bug worth a
report with the raw `llama-server --list-devices` output attached.

**Software rasterisers.** `llvmpipe` / `lavapipe` are *marked, not hidden* — device selection skips
them unless you ask, but discovery keeps the information so you can see why a "GPU" is slow.

**Multi-GPU is in the unverified bucket.** `-sm row`, `--tensor-split`, `--main-gpu` and multi-device
`-dev` are emitted by the one argv builder and asserted against a fake `llama-server`. No multi-GPU
box has ever run them. If you have one, this is the single most valuable thing you can test — and
`apexrouter endpoint argv <id>` will show you the exact argv before you commit to it.

### The daemon

**Port already in use.** `apexrouter doctor --only ports`. Change `[server] proxy_bind` /
`control_bind`, or set `$PROXY_PORT` (honoured because LocalRouter honoured it and shell aliases
depend on it).

**"daemon not running" but something is clearly listening.** The lock file
`$STATE/apexrouterd.lock` holds the owner record (pid, start time, boot id, control URL) and
liveness is *computed* from it, never read from a status string on disk. `apexrouter serve --stop`
tidies up. If a lock survives a hard kill, the identity check will notice the pid no longer matches
and take over.

**Nothing on stdout.** By design. The daemon writes **zero bytes** to stdout even at `-vv`, because
`apexrouter mcp` shares the binary and owns stdout for JSON-RPC. All logging is on stderr; under
systemd, `journalctl --user -u apexrouter`.

**`| jq` fails on a CLI command.** Use the subcommand's `--json` flag. With it, stdout is the
protocol type and nothing else, and error paths write **0 bytes** to stdout. Without it you get the
human table.

**A config key I set does nothing.** It warns on stderr the moment the file is read, and
`apexrouter config show --json | jq .unknown_keys` lists it with the key it was probably meant to be.
`proxy_port` is the classic — the key is `proxy_bind`.

**`apexrouter config validate` does not exist.** Known, filed as a LOW open item in the release
notes. Use `config show --json | jq .unknown_keys` until it lands.

### Requests

**`404` on a model name.** `[router] unknown_model = "reject"` is the default and is deliberate: a
fat-fingered `gpt-4o-mimi` should fail loudly rather than silently bill a rented H100. Use `auto`,
check `apexrouter route ls`, or set `unknown_model = "fallback"` if you'd rather it guessed.

**Claude Code gets `400 tool translation is off`.** Set `[router] anthropic_tools = true` (it is the
default since mk1). Claude Code sends 92 tool definitions on *every* request, so with translation
off the Anthropic ingress does not work at all for the client it exists to serve.

**`503 no_healthy_backend` during a swap.** Should not happen: requests **park** on the alias while
a sequential swap loads, and the park re-arms on the launch still making progress rather than on a
stopwatch. If you exceed `[router] warm_queue_max` (default 32) you get an honest
`503 warm_queue_full` **with a `Retry-After`** — a deeper queue only moves the failure later. If you
see plain `no_healthy_backend` mid-swap, that is a regression of the headline mk1 fix and very much
worth reporting, with the swap duration.

**A big model takes minutes to load and I expected a timeout.** `health_deadline_ms` is not a launch
budget — it is how long a launch may make **no progress**. The gate resets on every
`503 {"status":"loading model"}`, so a 12 s load passes a 1 s deadline. The longest load ever tested
here is 12 s (fake) / 7.4 s (real 7 GB Carnice). **A 30 GB GGUF whose mmap takes minutes is exactly
the case this was designed for and exactly the case never run.** Please report what happens.

**`X-Usage` is missing on a streaming response.** By design, and tested (CHARTER D8). Response
headers flush before the first SSE chunk and usage arrives in the last one, so a streaming `X-Usage`
would be absent or a lie. Streams get `X-ApexRouter-Usage-Deferred: true`, and the numbers land in
`usage.jsonl`, the WebSocket event and the live-request table. Its format on buffered responses is
LocalRouter's `"{prompt}+{completion}"`, not JSON.

**`403 redacted_endpoint` on `/slots`.** Deliberate. `/slots` echoes prompts and is never proxied
outward.

**`POST /health` behaves oddly.** It is *proxied*, not answered locally, and specifically not a
`405`. The proxy contract is that everything which is not one of five (path, method) pairs goes
upstream.

**A restart didn't kill my model.** Also by design (`kill_children_on_exit = false`): children are
`setsid()`'d and re-adopted. Use `apexrouter endpoint stop <id>`. If a restart *did* kill it, you
are missing `KillMode=process` in your systemd unit — [§8](#8-running-it-as-a-service-systemd---user).

### GUIs

**The web UI is blank or 404s.** It is served from the **control** port (2739), not the proxy port.
`[server] ui_dir = ""` serves the copy embedded in the binary; a path serves that directory instead
(the dev loop for `ui-web/`). Check `apexrouter status` for the real control URL — it can move.

**The native app says "not connected".** It resolves the control URL from `$APEXROUTER_URL`, then
`[server] control_bind`. If you moved `control_bind` and the app was built before mk1's fix, it
looked only at the env var. Set `APEXROUTER_URL=http://127.0.0.1:<port>` to be certain.

**Screenshotting the Slint app headlessly captures nothing.** Under `Xvfb`, winit prefers Wayland
and silently opens on your real desktop where X11 capture sees nothing. Force it:

```sh
env -u WAYLAND_DISPLAY WINIT_UNIX_BACKEND=x11 apexrouter-ui
```

### Remote and money

**A non-loopback bind refuses to start.** Correct, and it tells you how to fix it: a token must be
present in the variable named by `[server] token_env` (default `APEXROUTER_TOKEN`).
`apexrouter token create`.

**A browser can talk to my loopback daemon.** That is why every mutation on both listeners passes a
gate — `Host` allowlist (closing DNS rebinding), same-origin `Origin`/`Sec-Fetch-Site` when present,
bearer with `write` scope otherwise. There is **no CORS layer** on the authenticated API, and there
will not be (a cross-origin `fetch` with `Content-Type: text/plain` is a CORS *simple request*,
delivered without preflight, and the attacker never needs to read the response).

**`together.ratelimits` fails in `doctor`.** Expected if you have no together.ai key, or if
`[providers.together] base_url` points somewhere closed. In this project's own hermetic test runs it
fails **by construction**, because hermeticity requires it to point at a closed loopback port.

**vast.ai anything.** The entire rental lifecycle is in the unverified bucket by *rule*: this project
never calls a vast.ai endpoint that creates, modifies or destroys an instance. `vast offers`,
`vast ls` and `vast account` are read-only and are proven safe. Everything past `vast rent` — the
boot-phase state machine, `vast watch`, `vast destroy`'s verify-before-forget, the `max_boot_secs`
watchdog, startup reconciliation against a live account — exists, compiles, is unit-tested against
fakes, and **has never billed a real dollar**. If you rent a box with it, set
`[vast] max_usd_per_hour_ceiling` first, watch `apexrouter vast ls` and `apexrouter usage`, and
please report what you see. `vast ls` answers from the ledger even with the daemon down, which is
the property you want at exactly the moment you need it.

---

## 17. When something is genuinely wrong

Useful in a report, roughly in order of value:

```sh
apexrouter version
apexrouter doctor --json
apexrouter rig --json
apexrouter status --json
apexrouter endpoint argv <id>        # the exact argv, or what it would be
journalctl --user -u apexrouter -n 200 --no-pager
apexrouter endpoint logs <id> | tail -100
uname -a; rustc --version
```

`endpoint argv` is worth special mention: it renders a running child from its **record**, not by
re-planning, so it describes what actually happened rather than what would happen now. Compare it
against `/proc/<pid>/cmdline` if you suspect it is lying — that comparison is how the mk1 gate found
it *was*.

Redact before posting: `doctor --json` names credential *sources* (paths and env var names), never
values, but a path can still say more about your machine than you'd like.

---

## 18. What is not verified

Repeated here because it is the most important paragraph in this document for a field tester.

Everything below **exists, compiles, and is unit-tested against fakes. None of it has ever run
against the real thing.** Nothing is known broken; it is simply unproven.

- **Multi-GPU** — `-sm row`, `--tensor-split`, `--main-gpu`, multi-device `-dev`.
- **The vast.ai rental lifecycle** — everything past a read-only query, by rule.
- **SSH tunnel supervision against a real remote** — reconnect backoff, `ExitOnForwardFailure` on a
  real dead link, ControlMaster teardown, recycled `sshN.vast.ai` host keys.
- **Download-stall detection and recovery** — the `/proc/net/dev` RX delta over SSH and the one-click
  restart.
- **Large models and long loads** — the longest ever exercised is 12 s.
- **vLLM** — `endpoint vllm` has never launched a real vLLM.
- **Live together.ai and HuggingFace** — hermeticity means real rate-limit headers and a real large
  `hf get` are unexercised.
- **Non-loopback operation** with a real LAN peer and a real browser.
- **Adoption across a reboot** — restart-adoption is verified; reboot-adoption is not.
- **`Backend::Metal`** — in the enum so the data model need not change later. Nothing pretends it
  works.

The full list, with the reasoning:
[`docs/RELEASE-NOTES-mk1.md` §6.3](RELEASE-NOTES-mk1.md#63-waiting-on-real-hardware--the-honest-list).

If you have hardware that touches any of these, you can find something in an afternoon that this
project cannot find at all.

---

## See also

| | |
|---|---|
| [`README.md`](../README.md) | what it is and why |
| [`docs/RELEASE-NOTES-mk1.md`](RELEASE-NOTES-mk1.md) | what shipped, what was measured, **what is unverified** |
| [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) | normative design |
| [`docs/API.md`](API.md) | REST, WebSocket and CLI `--json` reference |
| [`docs/ROUTING.md`](ROUTING.md) | aliases, chains, strategies, failover |
| [`docs/AGENTS.md`](AGENTS.md) | MCP registration per harness, and the 24 tools |
| [`docs/MIGRATION.md`](MIGRATION.md) | coming from LocalRouter |
| [`docs/LICENSING.md`](LICENSING.md) | MIT OR Apache-2.0, and the one GPL crate |
| [`config.example.toml`](../config.example.toml) | every key, commented, at its default |
