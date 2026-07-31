#!/usr/bin/env bash
# ╔══════════════════════════════════════════════════════════════════════════════╗
# ║              ApexRouter-RS — installer                                       ║
# ║  One base URL for every model you can reach — local, rented, or managed.     ║
# ║  Installs: apexrouter (CLI + daemon + MCP server) · web UI (embedded)        ║
# ╚══════════════════════════════════════════════════════════════════════════════╝
#
# Quick install — UNATTENDED (this is what a pipe gets):
#   curl -fsSL https://raw.githubusercontent.com/buckster123/ApexRouter-RS/main/install.sh | bash
#   A piped install has no keyboard, so it runs fully hands-free: auto-detects the
#   toolchain, the source tree, your llama.cpp builds and your credentials, and
#   never opens a menu that could hang on a pipe. Same thing as `--yes`.
#
# Full manual, step-by-step — you choose every knob, with the detected answer
# offered as the default and a sentence on what it affects:
#   curl -fsSLO https://raw.githubusercontent.com/buckster123/ApexRouter-RS/main/install.sh
#   bash install.sh --tui              # …or force it even when piped:
#   curl -fsSL … | bash -s -- --tui    # (reads the keyboard from /dev/tty)
#
# From a local checkout (no network needed for the source):
#   cd ApexRouter-RS && ./install.sh              # the checkout is auto-detected
#   ./install.sh --from-source /path/to/ApexRouter-RS
#   On a terminal that is ONE menu (Automatic / Manual), then the plan, then
#   "Proceed? [Y/n]" — not unattended, and not the full manual walk. `--yes`
#   skips both; `--tui` takes the long way round. Same account as --help.
#
# NO SUDO BY DEFAULT. ApexRouter is a user-level tool, not a device OS:
#   binaries → ~/.local/bin           (--prefix to move; --system for /usr/local)
#   state    → ~/.local/state/apexrouter   (ledger, usage, logs, credentials 0600)
#   config   → ~/.config/apexrouter/config.toml   (seeded only if absent)
#   service  → systemd **--user** unit, no root, no /etc
#   log      → a temp file while it runs, copied to $STATE/install.log at the end
# Root is asked for exactly once and only if you pass --system, and then only to
# copy two files into /usr/local/bin. State, config, credentials and the service
# stay in your home at every setting — they are yours, not the machine's.
#
# The unit sets KillMode=process on purpose: llama-server children are spawned
# with setsid() to OUTLIVE the manager, and setsid() does not escape the cgroup,
# so systemd's default would kill the model you waited ninety seconds to load.
# Unattended runs also enable linger (a user service otherwise stops at logout);
# --no-linger declines, and `loginctl disable-linger $USER` undoes it.
#
# WHAT IT DOES NOT DO, on purpose:
#   * It never compiles llama.cpp. That is a long, hardware-specific job with
#     backend flags only you can choose. Existing builds are discovered; if there
#     are none you get the options, not a surprise 40-minute build.
#   * It never builds the native GUI unless you ask (--with-gui). That binary
#     links Slint under its GPL option and is therefore GPL-3.0-only; the daemon,
#     proxy, web UI, CLI and MCP server are MIT OR Apache-2.0 and stay that way.
#     See docs/LICENSING.md. The web UI is embedded in the daemon and needs nothing.
#   * It never overwrites an existing config.toml, and never touches
#     ~/.vastai-gguf (another tool's state directory).
#   * It never copies a credential. Keys found in your vast/HF/together config
#     stay where they are; the installer shows them masked, with provenance.
#
# Idempotency: the resolved choices are saved to $STATE/install.conf and restored
# on every re-run, so re-running upgrades the install instead of re-deciding it.
# Precedence: CLI flag > install.conf > auto-detect.
#
# Options: run with --help.

# ── This script is bash, and says so before it says anything else ──────────────
# `sh install.sh` is a thing people do, and /bin/sh is dash on Debian and Ubuntu.
# Everything below this line — [[ ]], arrays, ${x//y}, `local -a`, `set -o
# pipefail` — is a syntax error there, so a stranger's first contact with this
# project would be forty lines of `[[: not found` followed by a bogus "Install
# failed (exit 2) / What broke: starting up". A POSIX shell reads and runs one
# command at a time, so as long as THIS is the first statement in the file, it
# re-execs before the parser ever reaches bash-only syntax.
#
# Two failure shapes are handled, not one: a script on disk is re-run under
# bash, but `curl … | sh` has no file to re-run (the script is stdin, and it is
# already half-consumed), so that gets one honest sentence instead.
if [ -z "${BASH_VERSION:-}" ]; then
  if [ -f "$0" ] && [ -r "$0" ]; then
    exec bash "$0" "$@"
  fi
  echo "ApexRouter-RS installer: this script is bash, and it was started with a" >&2
  echo "POSIX shell that cannot parse it. Nothing was changed." >&2
  echo "  Piped:      curl -fsSL <url> | bash        (not | sh)" >&2
  echo "  From disk:  bash install.sh" >&2
  exit 1
fi

set -euo pipefail

# ── Identity ───────────────────────────────────────────────────────────────────
readonly PRODUCT="ApexRouter-RS"
readonly BIN_NAME="apexrouter"
readonly GUI_BIN_NAME="apexrouter-ui"
readonly SERVICE_NAME="apexrouter"
readonly DEFAULT_REPO_URL="https://github.com/buckster123/ApexRouter-RS"
readonly DEFAULT_PROXY_PORT=8888
readonly DEFAULT_CONTROL_PORT=2739

# ── Colours ────────────────────────────────────────────────────────────────────
# Captured BEFORE any stdout redirection, so a tee'd log does not silently turn
# the terminal monochrome (and so the progress lines know where they are going).
IS_TTY=false
[[ -t 1 ]] && IS_TTY=true
HAS_TTY_DEV=false
[[ -e /dev/tty ]] && (: >/dev/tty) 2>/dev/null && HAS_TTY_DEV=true

if $IS_TTY; then
  RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
  CYAN='\033[0;36m'; BOLD='\033[1m'; DIM='\033[2m'; NC='\033[0m'
else
  RED=''; GREEN=''; YELLOW=''; CYAN=''; BOLD=''; DIM=''; NC=''
fi

ok()   { echo -e "${GREEN}  +${NC} $*"; }
info() { echo -e "${CYAN}  ->${NC} $*"; }
warn() { echo -e "${YELLOW}  !${NC} $*"; }
note() { echo -e "${DIM}     $*${NC}"; }
die()  { echo -e "${RED}  x${NC} $*" >&2; exit 1; }
hdr()  { echo -e "\n${CYAN}${BOLD}---  $*  ---${NC}\n"; }
# Interactive prompts render on stderr, never stdout: the plain-text menus are
# read back with $(…) by their callers, so a header echoed to stdout would be
# captured as part of the answer. (That bug is easy to write and hard to see.)
say()  { echo -e "$*" >&2; }

# ── Failure reporting ──────────────────────────────────────────────────────────
# Every failure path says what broke, what it tried, and what to do next. The
# audience is a stranger on hardware we have never seen.
STEP="starting up"
STEP_TRIED=""
STEP_NEXT=""
step() { STEP="$1"; STEP_TRIED="${2:-}"; STEP_NEXT="${3:-}"; }

# The live log is a temp file for the whole run, because the state directory is
# a CHOICE the user may still be making. It is copied into $STATE_DIR/install.log
# on the way out — success or failure — and never causes that directory to be
# created just to hold a log.
LOG=""            # the live (temp) file
LOG_FINAL=""      # where it ends up, once the state dir is known and exists

# shellcheck disable=SC2329  # called by on_exit, which is a trap handler
archive_log() {
  [[ -n "$LOG" && -f "$LOG" ]] || return 0
  [[ -n "${STATE_DIR:-}" && -d "$STATE_DIR" && -w "$STATE_DIR" ]] || return 0
  LOG_FINAL="$STATE_DIR/install.log"
  cat "$LOG" >>"$LOG_FINAL" 2>/dev/null || { LOG_FINAL=""; return 0; }
  return 0
}

# "The install did not do its job" has two shapes and they need different words:
# nothing got installed, or it got installed and is not serving. Both exit
# non-zero — but telling someone their install failed when the binaries are in
# ~/.local/bin sends them looking in the wrong place.
VERIFY_FAILED=false

# shellcheck disable=SC2329  # invoked by `trap on_exit EXIT`, below
on_exit() {
  local rc=$?
  # whiptail can leave the terminal in raw mode when it is interrupted.
  $HAS_TTY_DEV && { stty sane </dev/tty 2>/dev/null || true; }
  archive_log
  (( rc == 0 )) && return 0
  echo "" >&2
  if $VERIFY_FAILED; then
    echo -e "${RED}${BOLD}  Installed, but not verified${NC} (exit $rc)" >&2
  else
    echo -e "${RED}${BOLD}  Install failed${NC} (exit $rc)" >&2
  fi
  echo -e "  ${BOLD}What broke:${NC}  $STEP" >&2
  [[ -n "$STEP_TRIED" ]] && echo -e "  ${BOLD}It tried:${NC}    $STEP_TRIED" >&2
  [[ -n "$STEP_NEXT"  ]] && echo -e "  ${BOLD}Next:${NC}        $STEP_NEXT" >&2
  [[ -n "${LOG_FINAL:-}" ]] && echo -e "  ${BOLD}Full log:${NC}    $LOG_FINAL" >&2
  [[ -z "${LOG_FINAL:-}" && -n "$LOG" ]] && echo -e "  ${BOLD}Full log:${NC}    $LOG" >&2
  echo -e "  ${DIM}Nothing that costs money was touched — this installer never calls a" >&2
  echo -e "  paid endpoint. Re-running is safe: it restores your choices and upgrades.${NC}" >&2
  echo "" >&2
  return 0
}
trap on_exit EXIT

# ══════════════════════════════════════════════════════════════════════════════
#  Small helpers
# ══════════════════════════════════════════════════════════════════════════════

have() { command -v "$1" >/dev/null 2>&1; }

# Echo the value of KEY= in an env-style file (handles `export `, quotes, CRLF).
_envval() {
  [[ -r "$1" ]] || return 0
  grep -aE "^[[:space:]]*(export[[:space:]]+)?$2=" "$1" 2>/dev/null \
    | head -1 | sed -E 's/^[^=]*=//; s/^["'"'"']//; s/["'"'"']$//' | tr -d ' \r' || true
}

# Truthy test for config flags: yes/y/1/true/on (case-insensitive).
_truthy() { [[ "$1" =~ ^([Yy]([Ee][Ss])?|1|[Tt][Rr][Uu][Ee]|[Oo][Nn])$ ]]; }

# One scalar out of a small, flat JSON object, by key. Good enough for exactly two
# things and nothing else: the owner record the daemon writes into its lock file
# (pretty-printed, one key per line) and the flat head of a `--json` envelope.
# Anything nested is not this function's business — the binary's own parser is.
# $1=json text $2=key
json_field() {
  printf '%s' "$1" \
    | grep -oE "\"$2\"[[:space:]]*:[[:space:]]*(\"[^\"]*\"|-?[0-9]+(\.[0-9]+)?|true|false)" \
    | head -1 | sed -E 's/^[^:]*:[[:space:]]*//; s/^"//; s/"$//' || true
}

# One value out of one TOML table. Approximate by design: this is used ONLY to
# find where a credential lives and to read a port, never to decide behaviour —
# the daemon parses these files with a real TOML parser and its answer wins.
# $1=file $2=table (e.g. providers.together, without brackets) $3=key
_toml_val() {
  [[ -r "$1" ]] || return 0
  awk -v want="$2" -v want_key="$3" '
    function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t\r]+$/, "", s); return s }
    {
      line = trim($0)
      if (line ~ /^#/ || line == "") next
      if (line ~ /^\[/) {
        sec = line
        sub(/^\[+/, "", sec); sub(/\]+.*$/, "", sec)
        gsub(/"/, "", sec)
        insec = (sec == want)
        next
      }
      if (!insec) next
      eq = index(line, "=")
      if (eq == 0) next
      k = trim(substr(line, 1, eq - 1)); gsub(/"/, "", k)
      if (k != want_key) next
      v = trim(substr(line, eq + 1))
      if (v ~ /^"/) { sub(/^"/, "", v); sub(/".*$/, "", v) }
      else          { sub(/[ \t]*#.*$/, "", v); v = trim(v) }
      print v
      exit
    }' "$1" 2>/dev/null || true
}

# Expand a leading ~ against $HOME. Anything else is returned unchanged.
# The quoted "~" patterns are the point: these match a LITERAL tilde that arrived
# as data (out of a config file, or a --prefix the shell never got to expand),
# which is exactly the case the shell will not expand for us.
# shellcheck disable=SC2088  # matching a literal ~, not relying on expansion
expand_tilde() {
  case "$1" in
    "~")   printf '%s' "$HOME" ;;
    "~/"*) printf '%s/%s' "$HOME" "${1#\~/}" ;;
    *)     printf '%s' "$1" ;;
  esac
}

# Absolute path without requiring the target to exist.
abspath() {
  local p; p=$(expand_tilde "$1")
  case "$p" in
    /*) printf '%s' "$p" ;;
    *)  printf '%s/%s' "$PWD" "$p" ;;
  esac
}

# version_ge A B  ->  true when A >= B, semver-ish, via sort -V.
version_ge() {
  [[ "$1" == "$2" ]] && return 0
  [[ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" == "$2" ]]
}

# Mask a credential for display — enough to recognise, never enough to leak.
mask_key() {
  local k="$1"
  if (( ${#k} <= 12 )); then printf '***'
  else printf '%s...%s' "${k:0:6}" "${k: -4}"
  fi
}

human_bytes() {
  local b="$1"
  if   (( b >= 1073741824 )); then printf '%d.%d GB' $(( b / 1073741824 )) $(( (b % 1073741824) * 10 / 1073741824 ))
  elif (( b >= 1048576 ));    then printf '%d MB' $(( b / 1048576 ))
  else                             printf '%d KB' $(( b / 1024 ))
  fi
}

# ══════════════════════════════════════════════════════════════════════════════
#  Arguments — parsed FIRST, so --help and --dry-run can touch nothing at all
# ══════════════════════════════════════════════════════════════════════════════

YES=false; TUI_FORCE=false; DRY_RUN=false; UNINSTALL=false; PURGE=false
OFFLINE=false; SYSTEM=false; WITH_GUI=false; WITH_SERVICE=true
MODIFY_PATH=true; WITH_COMPLETIONS=true; ALLOW_RUSTUP=true; DO_BUILD=true
SKIP_VERIFY=false; ENABLE_LINGER=""      # "" = ask/auto, true/false = decided
PREFIX=""; STATE_DIR=""; SRC_DIR=""; REPO_URL="$DEFAULT_REPO_URL"; BRANCH=""
JOBS=""

# Provenance markers — a CLI flag always wins over a stored install.conf. There is
# deliberately no STATE_CLI: the state dir is the one choice install.conf can never
# override, because install.conf LIVES in it (a record cannot tell you where to
# find the record), so there is nothing for a marker to arbitrate.
PREFIX_CLI=false; GUI_CLI=false; SERVICE_CLI=false
SRC_CLI=false; SYSTEM_CLI=false; JOBS_CLI=false; PATH_CLI=false

usage() {
  cat <<EOF
${PRODUCT} installer — one base URL for every model you can reach.

USAGE
  install.sh [options]
  curl -fsSL ${DEFAULT_REPO_URL}/raw/main/install.sh | bash            # unattended
  curl -fsSL ${DEFAULT_REPO_URL}/raw/main/install.sh | bash -s -- --tui # menus

MODE
  (no mode flag)        On a terminal: ONE menu (Automatic / Manual), then the
                        plan, then "Proceed? [Y/n]". Automatic uses the detected
                        answers; Manual adds eight questions before the plan.
                        Piped, there is no keyboard, so a pipe is unattended.
  -y, --yes             Unattended. Implied when stdin is not a terminal (a pipe).
      --tui             Full manual, step-by-step. Works even when piped.
      --dry-run         Print the complete plan and touch nothing. No writes, no
                        network, no build. This is how you audit it before running.

WHERE
      --prefix DIR      Install root for binaries (default: \$HOME/.local
                        -> \$HOME/.local/bin). Set to /usr/local for system-wide.
      --system          System-wide binaries in /usr/local/bin. Needs root, and
                        asks for it. State/config/service stay per-user — they
                        hold your credentials and your ledger.
      --state-dir DIR   \$APEXROUTER_HOME (default: \$HOME/.local/state/apexrouter).
                        Ledger, usage, logs, endpoint records, credentials.toml.

WHAT
      --with-gui        Also build the native Slint app (${GUI_BIN_NAME}).
                        That one binary is GPL-3.0-only and needs system dev
                        packages. Off by default. See docs/LICENSING.md.
      --no-gui          Explicitly skip it (the default).
      --no-service      Do not install/enable the systemd --user unit.
      --service         Install it (the default when systemd --user is available).
      --linger          loginctl enable-linger \$USER, so the daemon (and the
                        models it supervises) survive a logout. Unattended runs
                        do this by default when they install the service.
      --no-linger       Never call loginctl. Your user services then stop when
                        your last session ends.
      --no-completions  Do not write shell completions.
      --no-modify-path  Never touch a shell rc file, even if PREFIX/bin is not
                        on \$PATH (you get the export line to paste instead).
      --no-rustup       Never install a Rust toolchain; fail honestly if absent.
      --no-build        Do not run cargo; install an existing target/release build.

SOURCE
      --from-source DIR Use this checkout. No clone, no pull, no network.
      --repo-url URL    Clone from here instead (default: ${DEFAULT_REPO_URL}).
      --branch REF      Branch/tag to clone.
      --offline         No network at all: no clone, no rustup, no key checks.
      --jobs N          cargo build jobs (default: derived from RAM and cores).

OTHER
      --skip-verify     Do not start the daemon at the end.
      --uninstall       Remove binaries, unit and completions. Keeps your state
                        and config, and prints exactly what to delete if you
                        want them gone. Add --purge to delete them too (asks).
  -h, --help            This.

EXIT CODES
  0  installed AND verified — the daemon serving is the binary this run wrote.
     (also: a clean --dry-run, --help, and a --skip-verify install.)
  1  anything else, with a reason on stderr. Including "the files are on disk
     but it is not serving": a failed verification is a failed install, because
     the only thing you asked for was a working base URL.
EOF
}

while (( $# )); do
  case "$1" in
    -y|--yes)          YES=true ;;
    --tui)             TUI_FORCE=true ;;
    --dry-run)         DRY_RUN=true ;;
    --uninstall)       UNINSTALL=true ;;
    --purge)           PURGE=true ;;
    --offline)         OFFLINE=true ;;
    --system)          SYSTEM=true;  SYSTEM_CLI=true ;;
    --with-gui)        WITH_GUI=true;  GUI_CLI=true ;;
    --no-gui)          WITH_GUI=false; GUI_CLI=true ;;
    --service)         WITH_SERVICE=true;  SERVICE_CLI=true ;;
    --no-service)      WITH_SERVICE=false; SERVICE_CLI=true ;;
    --linger)          ENABLE_LINGER=true ;;
    --no-linger)       ENABLE_LINGER=false ;;
    --no-completions)  WITH_COMPLETIONS=false ;;
    --no-modify-path)  MODIFY_PATH=false; PATH_CLI=true ;;
    --no-rustup)       ALLOW_RUSTUP=false ;;
    --no-build)        DO_BUILD=false ;;
    --skip-verify)     SKIP_VERIFY=true ;;
    --prefix)          PREFIX="${2:-}"; PREFIX_CLI=true; shift ;;
    --prefix=*)        PREFIX="${1#*=}"; PREFIX_CLI=true ;;
    --state-dir)       STATE_DIR="${2:-}"; shift ;;
    --state-dir=*)     STATE_DIR="${1#*=}" ;;
    --from-source)     SRC_DIR="${2:-}"; SRC_CLI=true; shift ;;
    --from-source=*)   SRC_DIR="${1#*=}"; SRC_CLI=true ;;
    --repo-url)        REPO_URL="${2:-}"; shift ;;
    --repo-url=*)      REPO_URL="${1#*=}" ;;
    --branch)          BRANCH="${2:-}"; shift ;;
    --branch=*)        BRANCH="${1#*=}" ;;
    --jobs)            JOBS="${2:-}"; JOBS_CLI=true; shift ;;
    --jobs=*)          JOBS="${1#*=}"; JOBS_CLI=true ;;
    -h|--help)         usage; exit 0 ;;
    *)                 echo "unknown option: $1" >&2; echo "try: install.sh --help" >&2; exit 1 ;;
  esac
  shift
done

# `curl … | bash` has no keyboard on stdin (stdin IS the script), so an
# interactive menu there renders a box that can never read a keypress and hangs
# the console in raw mode. Unattended is therefore the default for a pipe; --tui
# forces the menus anyway and reads the keyboard from /dev/tty.
if ! $TUI_FORCE && { $YES || ! [ -t 0 ]; }; then
  YES=true
fi
$TUI_FORCE && YES=false

# …but --tui still needs a keyboard. Piped, with no /dev/tty to read from, there
# is no keyboard at all — reading stdin there would consume the script's own
# remaining text. Fall back to unattended and say why, rather than hanging on a
# prompt nobody can answer.
TUI_IMPOSSIBLE=false
if ! $YES && ! [ -t 0 ] && ! $HAS_TTY_DEV; then
  TUI_IMPOSSIBLE=true
  YES=true
fi

# ══════════════════════════════════════════════════════════════════════════════
#  Platform + path resolution
# ══════════════════════════════════════════════════════════════════════════════

step "detecting the platform"

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Linux) : ;;
  Darwin)
    die "macOS is not supported by mk1 and this installer will not pretend otherwise.
     The process model is Linux-specific: /proc identity, flock owner records,
     setsid() child adoption and boot_id. See docs/CHARTER.md D15.
     Next: run it on Linux, or open an issue — the data model already has room." ;;
  *)
    die "$OS is not a platform this build has ever run on (Linux only — CHARTER D15).
     Next: if you believe it should work, run the steps in docs/ by hand and
     tell us what broke." ;;
esac

RAM_KB=$(awk '/^MemTotal:/{print $2}' /proc/meminfo 2>/dev/null || echo 0)
RAM_MB=$(( RAM_KB / 1024 ))
NPROC=$(nproc 2>/dev/null || echo 2)
# $USER is not guaranteed to be exported (cron, some containers, `env -i`).
ME="${USER:-$(id -un 2>/dev/null || echo "$LOGNAME")}"

# Where things go. A user-level tool: no /etc, no /var, no sudo unless asked.
[[ -n "$PREFIX" ]] || { $SYSTEM && PREFIX="/usr/local" || PREFIX="$HOME/.local"; }
PREFIX="$(abspath "$PREFIX")"
BIN_DIR="$PREFIX/bin"

# The state dir is the anchor for everything else (install.conf lives in it), so
# it is resolved from flag > $APEXROUTER_HOME > XDG > default and never from
# install.conf itself — a record cannot tell you where to find the record.
DEFAULT_STATE="$(abspath "${XDG_STATE_HOME:-$HOME/.local/state}/apexrouter")"
if [[ -z "$STATE_DIR" ]]; then
  if [[ -n "${APEXROUTER_HOME:-}" ]]; then STATE_DIR="$APEXROUTER_HOME"
  else STATE_DIR="$DEFAULT_STATE"
  fi
fi
STATE_DIR="$(abspath "$STATE_DIR")"

# Config resolution must mirror core/src/paths.rs EXACTLY, because getting it
# wrong here is invisible and expensive:
#
#     $APEXROUTER_CONFIG -> $APEXROUTER_HOME/config.toml -> $XDG_CONFIG_HOME/apexrouter/config.toml
#
# Note the second link. Setting APEXROUTER_HOME — even to the value that is
# already the default — MOVES the config file the daemon reads, from
# ~/.config/apexrouter/config.toml to <state>/config.toml. So the rule is:
# **APEXROUTER_HOME is set only when the state dir is not the default one**, and
# the unit file below follows the same rule. Otherwise a stock install would
# silently stop reading a config the user had already written.
resolve_config_path() {
  STATE_IS_DEFAULT=false
  [[ "$STATE_DIR" == "$DEFAULT_STATE" && -z "${APEXROUTER_HOME:-}" ]] && STATE_IS_DEFAULT=true
  if [[ -n "${APEXROUTER_CONFIG:-}" ]]; then
    CONFIG_FILE="$APEXROUTER_CONFIG"
  elif $STATE_IS_DEFAULT; then
    CONFIG_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/apexrouter/config.toml"
  else
    CONFIG_FILE="$STATE_DIR/config.toml"
  fi
  CONFIG_FILE="$(abspath "$CONFIG_FILE")"
  CONF_FILE="$STATE_DIR/install.conf"
  return 0
}
resolve_config_path
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/apexrouter"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT_FILE="$UNIT_DIR/${SERVICE_NAME}.service"

# ══════════════════════════════════════════════════════════════════════════════
#  Stored choices — a re-run upgrades, it does not re-decide
# ══════════════════════════════════════════════════════════════════════════════

CONF_RESTORED=false
SRC_FROM_CONF=false
load_persisted_config() {
  [[ -f "$CONF_FILE" ]] || return 0
  local c
  c=$(_envval "$CONF_FILE" APEXROUTER_INSTALL_PREFIX)
  [[ -n "$c" ]] && ! $PREFIX_CLI && ! $SYSTEM_CLI && { PREFIX="$c"; BIN_DIR="$PREFIX/bin"; }
  c=$(_envval "$CONF_FILE" APEXROUTER_INSTALL_SYSTEM)
  [[ -n "$c" ]] && ! $SYSTEM_CLI && { _truthy "$c" && SYSTEM=true || SYSTEM=false; }
  c=$(_envval "$CONF_FILE" APEXROUTER_INSTALL_GUI)
  [[ -n "$c" ]] && ! $GUI_CLI && { _truthy "$c" && WITH_GUI=true || WITH_GUI=false; }
  c=$(_envval "$CONF_FILE" APEXROUTER_INSTALL_SERVICE)
  [[ -n "$c" ]] && ! $SERVICE_CLI && { _truthy "$c" && WITH_SERVICE=true || WITH_SERVICE=false; }
  c=$(_envval "$CONF_FILE" APEXROUTER_INSTALL_SOURCE)
  [[ -n "$c" && -d "$c" ]] && ! $SRC_CLI && { SRC_DIR="$c"; SRC_FROM_CONF=true; }
  c=$(_envval "$CONF_FILE" APEXROUTER_INSTALL_REPO)
  [[ -n "$c" && "$REPO_URL" == "$DEFAULT_REPO_URL" ]] && REPO_URL="$c"
  c=$(_envval "$CONF_FILE" APEXROUTER_INSTALL_JOBS)
  [[ -n "$c" ]] && ! $JOBS_CLI && JOBS="$c"
  c=$(_envval "$CONF_FILE" APEXROUTER_INSTALL_MODIFY_PATH)
  [[ -n "$c" ]] && ! $PATH_CLI && { _truthy "$c" && MODIFY_PATH=true || MODIFY_PATH=false; }
  CONF_RESTORED=true
}
load_persisted_config

PREV_VERSION="$(_envval "$CONF_FILE" APEXROUTER_INSTALL_VERSION)"

# ══════════════════════════════════════════════════════════════════════════════
#  Logging — set up after the args, so --help/--dry-run really do write nothing
# ══════════════════════════════════════════════════════════════════════════════

if ! $DRY_RUN; then
  LOG="$(mktemp -t apexrouter-install.XXXXXX.log 2>/dev/null || echo /tmp/apexrouter-install.log)"
  if : >>"$LOG" 2>/dev/null; then
    exec > >(tee -a "$LOG") 2>&1
  else
    LOG=""
  fi
  echo "-- ${PRODUCT} install started $(date -Is) --"
fi

# ══════════════════════════════════════════════════════════════════════════════
#  TUI — whiptail where present, plain text where not. Never on a bare pipe.
# ══════════════════════════════════════════════════════════════════════════════

HAVE_WHIPTAIL=false
if ! $YES && ! $DRY_RUN && $HAS_TTY_DEV \
   && [[ -z "${APEXROUTER_INSTALL_NO_WHIPTAIL:-}" ]]; then
  # whiptail draws on /dev/tty and reads the keyboard from /dev/tty. Without one
  # it renders a box that can never take a keypress, so the plain-text path wins.
  # Set APEXROUTER_INSTALL_NO_WHIPTAIL=1 to force plain text — useful on a dumb
  # terminal, under a screen reader, and when scripting the prompts.
  have whiptail && HAVE_WHIPTAIL=true
  # No sudo by default, so we never apt-get a dialog library. The plain-text
  # fallback below is a first-class path, not a degraded one.
fi
H=22; W=76

# stdin for prompts: /dev/tty when it exists (mandatory under `curl | bash --tui`).
_tty_in() { $HAS_TTY_DEV && printf '/dev/tty' || printf '/dev/stdin'; }

# (There was a tui_msg() here. Nothing ever called it: every message this script
# shows goes through ok/info/warn/note/hdr, which work identically piped and on a
# terminal. A prompt helper that no prompt uses is a whiptail invocation nobody
# has ever seen run, so it is gone rather than maintained untested.)

# Ask yes/no. $1=title $2=body $3=default(yes|no). Returns 0 for yes.
tui_yesno() {
  local def="${3:-yes}"
  if $HAVE_WHIPTAIL; then
    local extra=()
    [[ "$def" == "no" ]] && extra=(--defaultno)
    whiptail --title "$1" ${extra[@]+"${extra[@]}"} --yesno "$2" $H $W \
      3>&1 1>/dev/tty 2>&3 </dev/tty
    return $?
  fi
  say "\n${CYAN}${BOLD}---  $1  ---${NC}\n"
  say "$2"
  local prompt="[Y/n]"; [[ "$def" == "no" ]] && prompt="[y/N]"
  printf '%b' "${BOLD}  ? ${NC}$prompt " >&2
  local reply=""; read -r reply <"$(_tty_in)" || true
  [[ -z "$reply" ]] && reply="$def"
  [[ "$reply" =~ ^[Yy] ]]
}

# Single-select. Echoes the chosen tag ON STDOUT and nothing else.
# $1=title $2=prompt $3.. = tag desc pairs.
tui_menu() {
  local title="$1" prompt="$2"; shift 2
  if $HAVE_WHIPTAIL; then
    local choice
    choice=$(whiptail --title "$title" --menu "$prompt" $H $W $(( ($# / 2) + 1 )) "$@" \
      3>&1 1>/dev/tty 2>&3 </dev/tty) || true
    echo "$choice"
    return 0
  fi
  say "\n${CYAN}${BOLD}---  $title  ---${NC}\n"
  say "$prompt\n"
  local -a tags=()
  local i=1
  while (( $# >= 2 )); do
    say "    $i) $2"
    say "${DIM}       ($1)${NC}"
    tags+=("$1"); shift 2; i=$((i+1))
  done
  printf '%b' "\n${BOLD}  ? ${NC}number [1] " >&2
  local reply=""; read -r reply <"$(_tty_in)" || true
  [[ -z "$reply" ]] && reply=1
  [[ "$reply" =~ ^[0-9]+$ ]] || reply=1
  (( reply >= 1 && reply <= ${#tags[@]} )) || reply=1
  echo "${tags[$((reply-1))]}"
}

# Free text with a default. Echoes the value ON STDOUT and nothing else.
# $1=title $2=prompt $3=default $4=type(text|password)
tui_input() {
  local title="$1" prompt="$2" def="${3:-}" type="${4:-text}"
  if $HAVE_WHIPTAIL; then
    local val flag="--inputbox"
    [[ "$type" == "password" ]] && flag="--passwordbox"
    if [[ "$type" == "password" ]]; then
      val=$(whiptail --title "$title" "$flag" "$prompt" 12 $W 3>&1 1>/dev/tty 2>&3 </dev/tty) || val=""
    else
      val=$(whiptail --title "$title" "$flag" "$prompt" 12 $W "$def" 3>&1 1>/dev/tty 2>&3 </dev/tty) || val="$def"
    fi
    [[ -z "$val" && "$type" != "password" ]] && val="$def"
    echo "$val"
    return 0
  fi
  say "\n${CYAN}${BOLD}---  $title  ---${NC}\n"
  say "$prompt"
  local val=""
  if [[ "$type" == "password" ]]; then
    printf '%b' "${BOLD}  ? ${NC}" >&2
    read -rs val <"$(_tty_in)" || true; say ""
  else
    printf '%b' "${BOLD}  ? ${NC}[${def}] " >&2
    read -r val <"$(_tty_in)" || true
    [[ -z "$val" ]] && val="$def"
  fi
  echo "$val"
}

# ══════════════════════════════════════════════════════════════════════════════
#  Banner
# ══════════════════════════════════════════════════════════════════════════════

echo ""
echo -e "${CYAN}${BOLD}"
echo "   █████╗ ██████╗       ██████╗ ███████╗"
echo "  ██╔══██╗██╔══██╗      ██╔══██╗██╔════╝"
echo "  ███████║██████╔╝ ████╗██████╔╝███████╗"
echo "  ██╔══██║██╔══██╗      ██╔══██╗╚════██║"
echo "  ██║  ██║██║  ██║      ██║  ██║███████║"
echo "  ╚═╝  ╚═╝╚═╝  ╚═╝      ╚═╝  ╚═╝╚══════╝"
echo -e "${NC}"
echo -e "${DIM}  ${PRODUCT} — one base URL for every model you can reach.${NC}"
echo -e "${DIM}  local llama.cpp · rented vast.ai · together.ai · HuggingFace${NC}"
echo ""
if $TUI_IMPOSSIBLE; then
  warn "--tui was asked for, but stdin is a pipe and there is no /dev/tty to read"
  note "a keypress from. Running unattended instead (nothing will prompt)."
  note "For the menus: download the script first, then run 'bash install.sh --tui'."
fi

# ══════════════════════════════════════════════════════════════════════════════
#  Uninstall — early exit path
# ══════════════════════════════════════════════════════════════════════════════

do_uninstall() {
  hdr "Uninstall"

  # The repository ships a dedicated uninstaller. Where it exists it is the
  # authority and we hand over to it, flags and all — two implementations of
  # "what did we install" would eventually disagree, and the one that ships with
  # the source is the one that knows. The block below is the fallback for an
  # install that has no checkout on disk (curl | bash, then rm the clone).
  local delegate="" cand
  for cand in "${SRC_DIR:-}/uninstall.sh" "${SRC_DIR:-}/scripts/uninstall.sh"; do
    [[ -n "${SRC_DIR:-}" && -f "$cand" ]] && { delegate="$cand"; break; }
  done
  if [[ -n "$delegate" ]]; then
    info "handing over to $delegate — it owns uninstall in this checkout"
    local -a dargs=(--prefix "$PREFIX")
    # --state-dir MUST be forwarded. Without it the delegate falls back to the
    # DEFAULT state/config dirs and, under --purge, deletes an unrelated
    # install's data while leaving the state dir actually named here intact —
    # including its 0600 credentials.toml. `export APEXROUTER_HOME` happens far
    # below (~line 1817), long after this uninstall path returns, so the
    # environment cannot rescue us the way it does for an exported HOME.
    [[ -n "${STATE_DIR:-}" ]] && dargs+=(--state-dir "$STATE_DIR")
    $DRY_RUN && dargs+=(--dry-run)
    $PURGE   && dargs+=(--purge)
    $YES     && dargs+=(--yes)
    note "running: bash $delegate ${dargs[*]}"
    if bash "$delegate" "${dargs[@]}"; then
      return 0
    fi
    warn "$delegate exited non-zero — nothing further was attempted here."
    note "Run it directly to see why:  bash $delegate --help"
    return 1
  fi

  info "no uninstall.sh alongside the source — using the built-in fallback"
  note "(it removes only what this installer writes; the shipped uninstaller"
  note " knows about more, so prefer it when you have the checkout)"

  local -a removed=() kept=() f
  # One code path for the plan and for the deed, so a dry run cannot describe
  # something the real run would not do.
  remove_path() {
    [[ -e "$1" ]] || return 0
    removed+=("$1")
    $DRY_RUN && return 0
    rm -rf "$1"
    return 0
  }

  if have systemctl && systemctl --user show-environment >/dev/null 2>&1; then
    if $DRY_RUN; then
      note "would stop and disable the user unit ${SERVICE_NAME}.service"
    else
      systemctl --user stop    "$SERVICE_NAME" >/dev/null 2>&1 || true
      systemctl --user disable "$SERVICE_NAME" >/dev/null 2>&1 || true
      ok "user service stopped and disabled"
    fi
  fi
  remove_path "$UNIT_FILE"
  $DRY_RUN || systemctl --user daemon-reload >/dev/null 2>&1 || true

  # llama-server children are spawned with setsid() and deliberately OUTLIVE the
  # manager. Removing the manager does not kill your loaded models — say so, and
  # say how to stop them, rather than killing something that took 90 s and 6 GB.
  local live=""
  live=$(pgrep -a -f 'llama-server' 2>/dev/null | head -5 || true)

  for f in "$BIN_DIR/$BIN_NAME" "$BIN_DIR/$GUI_BIN_NAME" \
           "$PREFIX/share/bash-completion/completions/$BIN_NAME" \
           "$PREFIX/share/zsh/site-functions/_$BIN_NAME" \
           "$PREFIX/share/fish/vendor_completions.d/$BIN_NAME.fish" \
           "${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions/$BIN_NAME" \
           "${XDG_DATA_HOME:-$HOME/.local/share}/zsh/site-functions/_$BIN_NAME" \
           "${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions/$BIN_NAME.fish"; do
    remove_path "$f"
  done

  # The config lives inside the state dir on a non-default install; listing it
  # twice would suggest two separate deletions.
  local -a data=("$STATE_DIR")
  case "$CONFIG_FILE" in "$STATE_DIR"/*) : ;; *) data+=("$CONFIG_FILE") ;; esac

  if $PURGE; then
    local body="This deletes your ApexRouter state and config:

  $STATE_DIR
      ledger.jsonl (what you have spent), usage.jsonl, endpoint records,
      routes.json, logs, and credentials.toml if you ever typed a key in.
  $CONFIG_FILE

Keys you did NOT type stay where they always were (vast/HF/together configs)
- this installer never copied them anywhere.

Delete them?"
    local go=false
    if $YES; then go=true
    elif tui_yesno "Purge state and config" "$body" no; then go=true; fi
    if $go; then
      for f in "${data[@]}"; do remove_path "$f"; done
    else
      kept=("${data[@]}")
    fi
  else
    kept=("${data[@]}")
  fi

  echo ""
  if (( ${#removed[@]} )); then
    if $DRY_RUN; then info "Would remove:"; else ok "Removed:"; fi
    for f in "${removed[@]}"; do note "$f"; done
  else
    info "Nothing to remove — no ${PRODUCT} install found under $PREFIX"
  fi
  if (( ${#kept[@]} )); then
    echo ""
    info "Kept — this is your data. Delete it by hand if you want it gone:"
    for f in "${kept[@]}"; do [[ -e "$f" ]] && note "rm -rf $f"; done
  fi
  if [[ -n "$live" ]]; then
    echo ""
    warn "llama-server children are still running — they outlive the manager by design:"
    echo "$live" | while IFS= read -r l; do note "$l"; done
    note "Stop them with: kill <pid>   (they hold your loaded model in RAM/VRAM)"
  fi
  echo ""
  return 0
}

if $UNINSTALL; then
  step "uninstalling" "remove binaries, completions and the user unit" \
       "run the repository's uninstall.sh directly for the full flag set"
  do_uninstall
  if $DRY_RUN; then
    echo -e "${GREEN}${BOLD}  Dry run complete — nothing was removed.${NC}"
  else
    echo -e "${GREEN}${BOLD}  ${PRODUCT} uninstalled.${NC}"
  fi
  echo ""
  exit 0
fi

# ══════════════════════════════════════════════════════════════════════════════
#  Detection
# ══════════════════════════════════════════════════════════════════════════════

step "inspecting this machine" \
     "read /proc/meminfo, \$PATH, ~/llama.cpp, and your existing config"

hdr "This machine"

DISTRO="unknown"
if [[ -r /etc/os-release ]]; then
  # shellcheck source=/dev/null  # a machine-local file, read in a subshell for one string
  DISTRO="$( . /etc/os-release 2>/dev/null; echo "${PRETTY_NAME:-${NAME:-unknown}}" )"
fi
info "$DISTRO — $ARCH, ${NPROC} cores, $(( RAM_MB / 1024 )) GB RAM"

# ---- systemd --user ----------------------------------------------------------
HAVE_USER_SYSTEMD=false
if have systemctl && [[ -n "${XDG_RUNTIME_DIR:-}" ]] && systemctl --user show-environment >/dev/null 2>&1; then
  HAVE_USER_SYSTEMD=true
fi
if $HAVE_USER_SYSTEMD; then
  ok "systemd --user is available (the service will run as you, no root)"
else
  warn "no systemd --user session here — the service step will be skipped"
  note "That is normal in a container, or over ssh without a login session."
  note "Nothing is lost: 'apexrouter serve --detach' returns once /health answers,"
  note "and any mutating verb autostarts a daemon anyway."
  # A --service that cannot be honoured is refused out loud, never silently.
  $SERVICE_CLI && $WITH_SERVICE && warn "--service was asked for but there is no user systemd to install it into."
  WITH_SERVICE=false
fi

# ---- linger (does the daemon survive logout?) --------------------------------
LINGER_ON=false
if have loginctl; then
  [[ "$(loginctl show-user "$ME" -p Linger --value 2>/dev/null || echo no)" == "yes" ]] && LINGER_ON=true
fi

# ---- toolchain ---------------------------------------------------------------
CARGO_BIN=""
CARGO_VER=""
if have cargo; then CARGO_BIN="$(command -v cargo)"
elif [[ -x "$HOME/.cargo/bin/cargo" ]]; then CARGO_BIN="$HOME/.cargo/bin/cargo"
fi
if [[ -n "$CARGO_BIN" ]]; then
  CARGO_VER="$("$CARGO_BIN" --version 2>/dev/null | awk '{print $2}' || true)"
fi
HAVE_RUSTUP=false
have rustup && HAVE_RUSTUP=true
[[ -x "$HOME/.cargo/bin/rustup" ]] && HAVE_RUSTUP=true

# ---- source tree -------------------------------------------------------------
looks_like_repo() {
  [[ -n "${1:-}" && -f "$1/Cargo.toml" ]] || return 1
  grep -q 'apexrouter-cli' "$1/Cargo.toml" 2>/dev/null
}

SELF_DIR=""
if [[ -n "${BASH_SOURCE[0]:-}" && -f "${BASH_SOURCE[0]}" ]]; then
  SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || true)"
fi

SRC_ORIGIN=""
if $SRC_FROM_CONF && [[ -n "$SRC_DIR" ]] && looks_like_repo "$SRC_DIR"; then
  SRC_DIR="$(abspath "$SRC_DIR")"
  SRC_ORIGIN="the source recorded in install.conf"
elif [[ -n "$SRC_DIR" ]]; then
  SRC_DIR="$(abspath "$SRC_DIR")"
  looks_like_repo "$SRC_DIR" \
    || die "--from-source $SRC_DIR is not an ${PRODUCT} checkout.
     It tried: reading \$DIR/Cargo.toml and looking for the apexrouter-cli member.
     Next: point --from-source at the directory that holds Cargo.toml and crates/."
  SRC_ORIGIN="--from-source"
elif looks_like_repo "$SELF_DIR"; then
  SRC_DIR="$SELF_DIR"; SRC_ORIGIN="the checkout this script lives in"
elif looks_like_repo "$PWD"; then
  SRC_DIR="$PWD"; SRC_ORIGIN="the checkout you are standing in"
elif looks_like_repo "$DATA_DIR/src"; then
  SRC_DIR="$DATA_DIR/src"; SRC_ORIGIN="a previous clone"
else
  SRC_DIR="$DATA_DIR/src"; SRC_ORIGIN="clone from $REPO_URL"
fi
NEED_CLONE=false
looks_like_repo "$SRC_DIR" || NEED_CLONE=true

# MSRV comes out of the workspace manifest. Never guessed — if we cannot read it
# yet (a clone that has not happened), we say so instead of inventing a number.
MSRV=""
if looks_like_repo "$SRC_DIR"; then
  MSRV="$(_toml_val "$SRC_DIR/Cargo.toml" "workspace.package" "rust-version")"
fi

if [[ -n "$CARGO_VER" ]]; then
  ok "Rust toolchain: cargo $CARGO_VER  ($CARGO_BIN)"
else
  warn "no Rust toolchain found"
fi
if $NEED_CLONE; then
  info "source: $SRC_ORIGIN -> $SRC_DIR"
else
  ok "source: $SRC_DIR  ($SRC_ORIGIN)"
  [[ -n "$MSRV" ]] && note "workspace MSRV (from Cargo.toml): $MSRV"
fi

# ---- an existing install -----------------------------------------------------
EXISTING_BIN=""
EXISTING_VER=""
[[ -x "$BIN_DIR/$BIN_NAME" ]] && EXISTING_BIN="$BIN_DIR/$BIN_NAME"
SHADOW_BIN=""
if have "$BIN_NAME"; then
  SHADOW_BIN="$(command -v "$BIN_NAME")"
  [[ -z "$EXISTING_BIN" ]] && EXISTING_BIN="$SHADOW_BIN"
fi
if [[ -n "$EXISTING_BIN" ]]; then
  EXISTING_VER="$("$EXISTING_BIN" --version 2>/dev/null | awk '{print $2}' || true)"
  ok "existing install: $EXISTING_BIN${EXISTING_VER:+  (v$EXISTING_VER)}"
fi
if [[ -n "$SHADOW_BIN" && "$SHADOW_BIN" != "$BIN_DIR/$BIN_NAME" ]]; then
  warn "a different '$BIN_NAME' is earlier on your \$PATH: $SHADOW_BIN"
  note "After this install, that one still wins. Remove it, or put $BIN_DIR first."
fi

# ---- is a daemon running right now? -----------------------------------------
# Read the configured control bind rather than assuming the default: an operator
# who moved it would otherwise be told their running daemon is not there.
CONTROL_ADDR="127.0.0.1:${DEFAULT_CONTROL_PORT}"
CFG_CONTROL="$(_toml_val "$CONFIG_FILE" "server" "control_bind")"
[[ -n "$CFG_CONTROL" ]] && CONTROL_ADDR="${CFG_CONTROL/0.0.0.0/127.0.0.1}"
DAEMON_RUNNING=false
LOCK_FILE="$STATE_DIR/apexrouterd.lock"
if [[ -f "$LOCK_FILE" ]] && have curl; then
  if curl -fsS --max-time 2 -o /dev/null "http://${CONTROL_ADDR}/health" 2>/dev/null; then
    DAEMON_RUNNING=true
    info "a daemon is already answering on ${CONTROL_ADDR} — this will upgrade under it"
  fi
fi

# ---- llama.cpp ---------------------------------------------------------------
# The same shape the daemon globs (core/src/discover/builds.rs): build*/bin,
# build*, bin, and the root itself, under every configured root, plus $PATH.
LLAMA_FOUND=()
scan_llama() {
  local roots=("$HOME/llama.cpp" "$HOME/Projects/llama.cpp" "/usr/local/bin" "/opt/llama.cpp")
  # Honour extra roots the user already configured, so we report what the daemon
  # will actually see rather than what the defaults would.
  if [[ -r "$CONFIG_FILE" ]]; then
    local cfg_roots; cfg_roots=$(_toml_val "$CONFIG_FILE" "endpoints" "build_roots")
    [[ -n "$cfg_roots" ]] && note "config build_roots: $cfg_roots"
  fi
  local r p
  for r in "${roots[@]}"; do
    [[ -d "$r" ]] || continue
    for p in "$r"/build*/bin/llama-server "$r"/build*/llama-server "$r"/bin/llama-server "$r"/llama-server; do
      [[ -x "$p" ]] && LLAMA_FOUND+=("$p")
    done
  done
  if have llama-server; then
    p="$(command -v llama-server)"
    LLAMA_FOUND+=("$p")
  fi
  # de-dup, order preserved
  if (( ${#LLAMA_FOUND[@]} )); then
    local -a uniq=()
    for p in "${LLAMA_FOUND[@]}"; do
      local seen=false q
      for q in ${uniq[@]+"${uniq[@]}"}; do [[ "$q" == "$p" ]] && seen=true; done
      $seen || uniq+=("$p")
    done
    LLAMA_FOUND=(${uniq[@]+"${uniq[@]}"})
  fi
}
scan_llama

if (( ${#LLAMA_FOUND[@]} )); then
  ok "llama.cpp: ${#LLAMA_FOUND[@]} build(s) found"
  for p in "${LLAMA_FOUND[@]}"; do note "$p"; done
  note "Backend labels come from 'llama-server --list-devices' — 'apexrouter rig' shows them."
else
  warn "no llama.cpp build found"
fi

# ---- GPUs (informational; the fit solver does the real work) -----------------
GPU_SUMMARY=""
if have nvidia-smi; then
  GPU_SUMMARY="$(nvidia-smi --query-gpu=name,memory.total --format=csv,noheader 2>/dev/null \
                 | paste -sd'; ' - || true)"
elif have rocm-smi; then
  # rocm-smi pads its table with tabs; squeeze them and drop the field label.
  GPU_SUMMARY="$(rocm-smi --showproductname 2>/dev/null \
                 | grep -i 'card series' | head -4 \
                 | sed -E 's/.*[Cc]ard [Ss]eries:[[:space:]]*//; s/[[:space:]]+/ /g; s/^ //; s/ $//' \
                 | paste -sd'; ' - || true)"
fi
if [[ -z "$GPU_SUMMARY" && -d /sys/class/drm ]]; then
  DRM_N=$(find /sys/class/drm -maxdepth 1 -name 'renderD*' 2>/dev/null | wc -l || echo 0)
  (( DRM_N > 0 )) && GPU_SUMMARY="$DRM_N DRM render node(s)"
fi
if [[ -n "$GPU_SUMMARY" ]]; then
  info "GPUs: $GPU_SUMMARY"
else
  note "no GPU probe available (CPU inference still works)"
fi

# ---- ports -------------------------------------------------------------------
# "name (pid N)" for whoever is listening on $1, or nothing. `ss` renders that
# as users:(("python3",pid=162852,fd=3)); nobody needs the fd.
port_holder() {
  local port="$1"
  if have ss; then
    ss -ltnp "sport = :$port" 2>/dev/null | awk 'NR>1{print $NF; exit}' \
      | sed -E 's/.*\(\("([^"]+)",pid=([0-9]+).*/\1 (pid \2)/' || true
  elif have lsof; then
    lsof -nP -iTCP:"$port" -sTCP:LISTEN -Fcp 2>/dev/null \
      | sed -n 's/^c//p' | head -1 || true
  fi
}
# Both listeners, reported the same way. A busy 2739 used to be computed and then
# silently dropped, so a stranger whose control port was taken got no warning here
# and a thirty-second wait later — the failure with the least information at the
# point where the most was available.
PROXY_HOLDER="$(port_holder "$DEFAULT_PROXY_PORT" || true)"
CONTROL_HOLDER="$(port_holder "$DEFAULT_CONTROL_PORT" || true)"
if ! $DAEMON_RUNNING; then
  for _pp in "${DEFAULT_PROXY_PORT}:proxy_bind:${PROXY_HOLDER}" \
             "${DEFAULT_CONTROL_PORT}:control_bind:${CONTROL_HOLDER}"; do
    _port="${_pp%%:*}"; _rest="${_pp#*:}"; _key="${_rest%%:*}"; _who="${_rest#*:}"
    [[ -n "$_who" ]] || continue
    warn "port ${_port} is already held by: ${_who}"
    note "The daemon refuses to start on a busy port and names the holder. Free it,"
    note "or set [server] ${_key} in $CONFIG_FILE."
    note "This installer will NOT count that process as its own — see Verify."
  done
  unset _pp _port _rest _key _who
fi

# ══════════════════════════════════════════════════════════════════════════════
#  Credentials — discovered exactly as core/src/secret.rs resolves them
# ══════════════════════════════════════════════════════════════════════════════
#
# The one chain, in the one order:
#   1. $STATE/credentials.toml  [providers.<id>].api_key   (a key YOU typed)
#   2. the key file our config names (api_key_file / token_file)
#   3. the conventional third-party path
#   4. the environment variable
# Nothing here is ever copied anywhere. A borrowed key is described, not stored.

CRED_VALUE=""; CRED_SRC=""

_cred_from_file() {           # $1=path -> sets CRED_VALUE/CRED_SRC if non-empty
  local p="$1"
  [[ -n "$p" && -r "$p" ]] || return 1
  local v; v="$(tr -d ' \t\r\n' < "$p" 2>/dev/null || true)"
  [[ -n "$v" ]] || return 1
  CRED_VALUE="$v"; CRED_SRC="file: $p"
}
_cred_from_env() {            # $1=var name
  local v="${!1:-}"
  v="$(printf '%s' "$v" | tr -d ' \t\r\n')"
  [[ -n "$v" ]] || return 1
  CRED_VALUE="$v"; CRED_SRC="env: \$$1"
}
_cred_from_store() {          # $1=provider id, in $STATE/credentials.toml
  local v; v="$(_toml_val "$STATE_DIR/credentials.toml" "providers.$1" "api_key")"
  [[ -n "$v" ]] || return 1
  CRED_VALUE="$v"; CRED_SRC="managed: $STATE_DIR/credentials.toml"
}

discover_vast() {
  CRED_VALUE=""; CRED_SRC=""
  _cred_from_store vast && return 0
  local f; f="$(_toml_val "$CONFIG_FILE" "vast" "api_key_file")"
  [[ -n "$f" ]] && _cred_from_file "$(expand_tilde "$f")" && return 0
  _cred_from_file "$HOME/.config/vastai/vast_api_key" && return 0
  _cred_from_env VAST_API_KEY && return 0
  _cred_from_env VASTAI_API_KEY && return 0
  return 1
}

discover_hf() {
  CRED_VALUE=""; CRED_SRC=""
  _cred_from_store hf && return 0
  local f; f="$(_toml_val "$CONFIG_FILE" "hf" "token_file")"
  [[ -n "$f" ]] && _cred_from_file "$(expand_tilde "$f")" && return 0
  # huggingface_hub's own order: $HF_TOKEN_PATH, $HF_HOME/token, <cache>/huggingface/token
  local hp="${HF_TOKEN_PATH:-}"
  [[ -z "$hp" && -n "${HF_HOME:-}" ]] && hp="$HF_HOME/token"
  [[ -z "$hp" ]] && hp="${XDG_CACHE_HOME:-$HOME/.cache}/huggingface/token"
  _cred_from_file "$hp" && return 0
  _cred_from_env HF_TOKEN && return 0
  _cred_from_env HUGGING_FACE_HUB_TOKEN && return 0
  return 1
}

TOGETHER_BASE="https://api.together.ai/v1"
discover_together() {
  CRED_VALUE=""; CRED_SRC=""
  local b; b="$(_toml_val "$CONFIG_FILE" "providers.together" "base_url")"
  [[ -n "$b" ]] && TOGETHER_BASE="$b"
  _cred_from_store together && return 0
  local f; f="$(_toml_val "$CONFIG_FILE" "providers.together" "api_key_file")"
  [[ -n "$f" ]] && _cred_from_file "$(expand_tilde "$f")" && return 0
  # LocalRouter's own config, read-only and never rewritten. Its base_url is used
  # verbatim: api.together.xyz must not silently become api.together.ai.
  local legacy="$HOME/.vastai-gguf/config.toml"
  local compat; compat="$(_toml_val "$CONFIG_FILE" "compat" "read_legacy_state")"
  if [[ -r "$legacy" && "$compat" != "false" ]]; then
    local lk lb
    lk="$(_toml_val "$legacy" "providers.together" "api_key")"
    lb="$(_toml_val "$legacy" "providers.together" "base_url")"
    if [[ -n "$lk" ]]; then
      [[ -n "$lb" ]] && TOGETHER_BASE="$lb"
      CRED_VALUE="$lk"; CRED_SRC="file: $legacy (read-only, never rewritten)"
      return 0
    fi
  fi
  local var; var="$(_toml_val "$CONFIG_FILE" "providers.together" "api_key_env")"
  [[ -z "$var" ]] && var="TOGETHER_API_KEY"
  _cred_from_env "$var" && return 0
  return 1
}

# Live verification that costs NOTHING. Each of these is an auth-only read:
#   vast     GET /users/current/   — free; the one live call the test suite makes
#   hf       GET /api/whoami-v2    — the cheapest possible token check
#   together GET <base>/models     — listing models consumes no tokens
# A valid-but-out-of-credit key still returns 200, which is exactly right: we are
# checking the KEY, not the balance. No response body is ever printed — vast's
# /users/current/ echoes the key back.
# Echoes: valid | invalid | offline
verify_key() {
  local url="$1" key="$2" code
  $OFFLINE && { echo offline; return 0; }
  have curl || { echo offline; return 0; }
  code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 12 \
           -H "Authorization: Bearer ${key}" "$url" 2>/dev/null || true)
  case "$code" in
    200|429) echo valid   ;;
    401|403) echo invalid ;;
    *)       echo offline ;;
  esac
}

VAST_KEY=""; VAST_SRC=""; VAST_STATE="absent"
HF_KEY="";   HF_SRC="";   HF_STATE="absent"
TOG_KEY="";  TOG_SRC="";  TOG_STATE="absent"

_cred_row() {  # $1=label $2=key $3=src $4=state
  if [[ -z "$2" ]]; then
    note "$(printf '%-10s %s' "$1" "not found — optional")"
    return 0
  fi
  local verdict=""
  case "$4" in
    valid)   verdict="${GREEN}verified${NC}" ;;
    invalid) verdict="${RED}REJECTED${NC}" ;;
    offline) verdict="${DIM}not checked${NC}" ;;
    pending) verdict="${DIM}will be checked after the plan${NC}" ;;
  esac
  echo -e "  $(printf '%-10s %-16s' "$1" "$(mask_key "$2")")  ${DIM}$3${NC}  $verdict"
}

# Discovery only: reads files and environment variables on this machine, and
# contacts nothing. The network half is verify_credentials(), below.
survey_credentials() {
  hdr "Credentials"
  note "Found, never copied. A key in your vast/HF/together config stays there;"
  note "only a key you explicitly type is ever written (0600, by the binary)."
  echo ""

  if discover_vast; then VAST_KEY="$CRED_VALUE"; VAST_SRC="$CRED_SRC"; fi
  if discover_hf;   then HF_KEY="$CRED_VALUE";   HF_SRC="$CRED_SRC";   fi
  if discover_together; then TOG_KEY="$CRED_VALUE"; TOG_SRC="$CRED_SRC"; fi

  local pending="offline"
  $CRED_WILL_CHECK && pending="pending"
  [[ -n "$VAST_KEY" ]] && VAST_STATE="$pending"
  [[ -n "$HF_KEY"   ]] && HF_STATE="$pending"
  [[ -n "$TOG_KEY"  ]] && TOG_STATE="$pending"

  _cred_row "vast.ai"  "$VAST_KEY" "$VAST_SRC" "$VAST_STATE"
  _cred_row "huggingf" "$HF_KEY"   "$HF_SRC"   "$HF_STATE"
  _cred_row "together" "$TOG_KEY"  "$TOG_SRC"  "$TOG_STATE"
  echo ""
  if $CRED_WILL_CHECK; then
    note "Nothing has been contacted yet. The plan below lists exactly which hosts"
    note "these keys will be offered to, and the checks run after you have seen it."
  else
    note "No live verification this run ($($DRY_RUN && echo '--dry-run' || echo '--offline / no curl'))."
  fi
  return 0
}

# The network half. Deliberately NOT called until the plan has been rendered (and,
# interactively, agreed to): an unattended pipe used to make three authenticated
# outbound calls before it had shown the reader a single line about what it was
# going to do, which is the one thing a `curl | bash` reader has no way to audit
# after the fact.
verify_credentials() {
  $CRED_WILL_CHECK || return 0
  [[ -n "$VAST_KEY$HF_KEY$TOG_KEY" ]] || return 0
  hdr "Credential check"
  note "Auth-only reads. Each costs nothing and spends nothing: vast's"
  note "/users/current/ is free, HF's whoami-v2 is a token echo, and listing"
  note "models consumes no tokens. No response body is ever printed."
  echo ""
  if [[ -n "$VAST_KEY" ]]; then
    VAST_STATE=$(verify_key "https://console.vast.ai/api/v0/users/current/" "$VAST_KEY")
  fi
  if [[ -n "$HF_KEY" ]]; then
    HF_STATE=$(verify_key "https://huggingface.co/api/whoami-v2" "$HF_KEY")
  fi
  if [[ -n "$TOG_KEY" ]]; then
    TOG_STATE=$(verify_key "${TOGETHER_BASE%/}/models" "$TOG_KEY")
  fi
  _cred_row "vast.ai"  "$VAST_KEY" "$VAST_SRC" "$VAST_STATE"
  _cred_row "huggingf" "$HF_KEY"   "$HF_SRC"   "$HF_STATE"
  _cred_row "together" "$TOG_KEY"  "$TOG_SRC"  "$TOG_STATE"
  echo ""
  [[ "$VAST_STATE" == invalid ]] && warn "vast.ai rejected that key — rentals will fail until it is replaced."
  [[ "$HF_STATE"   == invalid ]] && warn "HuggingFace rejected that token — gated repos and 'hf get' will fail."
  [[ "$TOG_STATE"  == invalid ]] && warn "together.ai rejected that key — the managed provider will 401."
  return 0
}

# Whether the network half will run at all — needed by the survey and by the plan,
# so it is decided once, here, before either of them speaks.
CRED_WILL_CHECK=false
if ! $DRY_RUN && ! $OFFLINE && have curl; then CRED_WILL_CHECK=true; fi

survey_credentials

# ══════════════════════════════════════════════════════════════════════════════
#  Choices — automatic, or the full manual walk
# ══════════════════════════════════════════════════════════════════════════════

# Build parallelism: rustc peaks around 1.5 GB per job on this workspace, and an
# OOM-killed build (signal 9) is the single most confusing failure a stranger can
# hit. Cap by RAM, then by cores.
derive_jobs() {
  [[ -n "$JOBS" ]] && return 0
  local by_ram=$(( RAM_MB / 1536 ))
  (( by_ram < 1 )) && by_ram=1
  JOBS=$(( NPROC < by_ram ? NPROC : by_ram ))
  (( JOBS < 1 )) && JOBS=1
  (( JOBS > 8 )) && JOBS=8
  # An arithmetic test that is merely false returns 1, and a function whose LAST
  # statement does that fails under `set -e`. Be explicit.
  return 0
}
derive_jobs

STYLE="auto"
if ! $YES && ! $DRY_RUN; then
  STYLE=$(tui_menu "${PRODUCT} installer" \
"Found this machine:

  $DISTRO
  $ARCH · ${NPROC} cores · $(( RAM_MB / 1024 )) GB RAM
  Rust: ${CARGO_VER:-none} · llama.cpp builds: ${#LLAMA_FOUND[@]} · systemd --user: $($HAVE_USER_SYSTEMD && echo yes || echo no)

How would you like to set it up?" \
    "auto"   "Automatic - use the detected answers (recommended)" \
    "manual" "Manual    - walk me through every choice")
  [[ -z "$STYLE" ]] && STYLE="auto"
fi

if [[ "$STYLE" == "manual" ]]; then
  hdr "Step by step"

  # 1. scope ------------------------------------------------------------------
  scope=$(tui_menu "Where do the binaries go?" \
"ApexRouter is a user-level tool: one daemon, your models, your keys, your
ledger. It does not want root and does not need it.

  user-level   binaries in ~/.local/bin, service as a systemd --user unit.
               Nothing outside your home directory is touched.
  system-wide  binaries in /usr/local/bin (needs sudo, once). State, config
               and the service stay per-user regardless - they hold your
               credentials and your spend history.

Current: $($SYSTEM && echo system-wide || echo user-level)" \
    "user"   "User-level  - no sudo, ~/.local/bin (recommended)" \
    "system" "System-wide - /usr/local/bin, asks for sudo once")
  case "$scope" in
    system) SYSTEM=true;  $PREFIX_CLI || { PREFIX="/usr/local"; BIN_DIR="$PREFIX/bin"; } ;;
    user)   SYSTEM=false; $PREFIX_CLI || { PREFIX="$HOME/.local"; BIN_DIR="$PREFIX/bin"; } ;;
  esac

  # 2. prefix -----------------------------------------------------------------
  PREFIX=$(tui_input "Install prefix" \
"Binaries land in <prefix>/bin. Anything works - ~/.local, /usr/local, or a
scratch directory you can delete.

Affects: where 'apexrouter' and (optionally) 'apexrouter-ui' are written, and
whether that directory needs to be added to your \$PATH." \
    "$PREFIX")
  PREFIX="$(abspath "$PREFIX")"; BIN_DIR="$PREFIX/bin"

  # 3. state ------------------------------------------------------------------
  STATE_DIR=$(tui_input "State directory" \
"Everything durable lives here, and nothing is ever written into the repo:

  ledger.jsonl     what you have spent, append-only
  usage.jsonl      tokens per request, from real timings
  endpoints/       one record per llama-server child (facts, never status)
  credentials.toml only if you ever type a key in - mode 0600
  logs/            child logs, rotated

Affects: \$APEXROUTER_HOME, and therefore where config.toml is read from -
anywhere other than the default moves the config next to the state." \
    "$STATE_DIR")
  STATE_DIR="$(abspath "$STATE_DIR")"
  resolve_config_path

  # 4. GUI --------------------------------------------------------------------
  gui_default=no; $WITH_GUI && gui_default=yes
  if tui_yesno "Native desktop app (GPL-3.0)" \
"The web UI is already inside the daemon - three files, no npm, no build step,
served at http://127.0.0.1:${DEFAULT_CONTROL_PORT}/. You do not need this to have a GUI.

The optional native app ('${GUI_BIN_NAME}') links the Slint toolkit under its
GPL option, so THAT ONE BINARY is GPL-3.0-only. The daemon, proxy, web UI, CLI
and MCP server are MIT OR Apache-2.0 and stay that way - the crate is kept out
of default-members so an ordinary build never links it.

It also needs system dev packages (libfontconfig1-dev and friends) and adds
several minutes to the build.

Build it?" "$gui_default"; then
    WITH_GUI=true
  else
    WITH_GUI=false
  fi

  # 5. service ----------------------------------------------------------------
  if $HAVE_USER_SYSTEMD; then
    svc_default=yes; $WITH_SERVICE || svc_default=no
    if tui_yesno "Run it as a service" \
"Install a systemd --user unit so the daemon starts with your session and comes
back after a crash. No root, no /etc - it lands in
  $UNIT_FILE

Two details the unit gets right, which matter here:
  KillMode=process   your llama-server children are spawned with setsid() and
                     are meant to OUTLIVE the manager. Without this, systemd
                     would kill the model you waited 90 seconds to load.
  linger             without it, systemd stops your user services at logout.

Install and enable it?" "$svc_default"; then
      WITH_SERVICE=true
    else
      WITH_SERVICE=false
    fi
    if $WITH_SERVICE && ! $LINGER_ON && have loginctl; then
      if tui_yesno "Keep it running after logout" \
"systemd stops user services when your last session ends, unless 'linger' is
enabled for your account. Turn it on so the daemon (and the models it is
supervising) survive a logout.

Runs: loginctl enable-linger $ME
Undo: loginctl disable-linger $ME" yes; then
        ENABLE_LINGER=true
      else
        ENABLE_LINGER=false
      fi
    fi
  fi

  # 6. PATH -------------------------------------------------------------------
  case ":$PATH:" in
    *":$BIN_DIR:"*) : ;;
    *)
      path_default=yes; $MODIFY_PATH || path_default=no
      if tui_yesno "Add $BIN_DIR to your PATH" \
"$BIN_DIR is not on your \$PATH, so 'apexrouter' would not be found by name.

Affects: one marked line appended to your shell rc file. Say no and the export
line is printed for you to paste instead." "$path_default"; then
        MODIFY_PATH=true
      else
        MODIFY_PATH=false
      fi ;;
  esac

  # 7. source -----------------------------------------------------------------
  if $NEED_CLONE; then
    src=$(tui_input "Where is the source?" \
"No ${PRODUCT} checkout was found next to this script.

  Leave blank to clone $REPO_URL into
    $DATA_DIR/src
  or give the path to a checkout you already have.

Affects: what gets built. Nothing is ever built from inside your state dir." "")
    if [[ -n "$src" ]]; then
      src="$(abspath "$src")"
      if looks_like_repo "$src"; then
        SRC_DIR="$src"; NEED_CLONE=false; SRC_ORIGIN="you named it"
        MSRV="$(_toml_val "$SRC_DIR/Cargo.toml" "workspace.package" "rust-version")"
      else
        warn "$src does not look like an ${PRODUCT} checkout — falling back to a clone"
      fi
    fi
  fi

  # 8. build jobs -------------------------------------------------------------
  JOBS=$(tui_input "Parallel build jobs" \
"This is a large workspace (eight crates, ~120k lines) and a release build is
minutes, not seconds. rustc peaks around 1.5 GB per job.

Detected: ${NPROC} cores, $(( RAM_MB / 1024 )) GB RAM -> $JOBS jobs.

Affects: build wall time, and whether the kernel OOM-killer stops rustc
(that shows up as 'signal: 9' and confuses everyone)." "$JOBS")
  [[ "$JOBS" =~ ^[0-9]+$ ]] || JOBS=1
fi

# GUI needs system dev packages. Check now so the plan can be honest about it.
GUI_LIBS_OK=true
GUI_LIBS_HINT=""
if $WITH_GUI; then
  if have pkg-config && pkg-config --exists fontconfig 2>/dev/null; then
    GUI_LIBS_OK=true
  else
    GUI_LIBS_OK=false
    if   have apt-get; then GUI_LIBS_HINT="sudo apt-get install -y libfontconfig1-dev pkg-config libxkbcommon-dev"
    elif have dnf;     then GUI_LIBS_HINT="sudo dnf install -y fontconfig-devel pkgconf-pkg-config libxkbcommon-devel"
    elif have pacman;  then GUI_LIBS_HINT="sudo pacman -S --needed fontconfig pkgconf libxkbcommon"
    elif have zypper;  then GUI_LIBS_HINT="sudo zypper install -y fontconfig-devel pkg-config libxkbcommon-devel"
    else GUI_LIBS_HINT="install the fontconfig development package for your distribution"
    fi
  fi
fi

# ══════════════════════════════════════════════════════════════════════════════
#  The plan — printed always, executed only when this is not a dry run
# ══════════════════════════════════════════════════════════════════════════════

# One plan row. `echo -e` (not printf) because the values carry colour escapes
# that were built as literal \033 strings.
plan_row() { echo -e "  ${BOLD}$(printf '%-12s' "$1")${NC} $2"; }

render_plan() {
  hdr "Plan"
  local rust_line
  if [[ -n "$CARGO_VER" ]]; then
    if [[ -n "$MSRV" ]] && ! version_ge "$CARGO_VER" "$MSRV"; then
      rust_line="cargo $CARGO_VER is older than the workspace MSRV $MSRV — will try 'rustup update stable'"
    else
      rust_line="the cargo already here — $CARGO_VER${MSRV:+, workspace MSRV $MSRV}"
    fi
  elif $ALLOW_RUSTUP && ! $OFFLINE; then
    rust_line="install rustup (~/.cargo + ~/.rustup, several hundred MB), then build"
  else
    rust_line="${YELLOW}NONE — and --no-rustup/--offline forbids installing one. This will fail.${NC}"
  fi

  local mode_line="interactive — one menu, then 'Proceed? [Y/n]'"
  $DRY_RUN && mode_line="interactive (a dry run asks nothing)"
  $YES     && mode_line="unattended — no prompts"
  plan_row "mode"        "$mode_line"
  plan_row "scope"       "$($SYSTEM && echo 'system-wide binaries — sudo for the copy only' || echo 'user-level — no sudo anywhere')"
  plan_row "binaries"    "$BIN_DIR/$BIN_NAME$($WITH_GUI && echo ", $BIN_DIR/$GUI_BIN_NAME" || true)"
  plan_row "state"       "$STATE_DIR   ${DIM}(\$APEXROUTER_HOME)${NC}"
  plan_row "config"      "$CONFIG_FILE   ${DIM}$([[ -f "$CONFIG_FILE" ]] && echo '(exists — kept, never overwritten)' || echo '(seeded from the commented example)')${NC}"
  plan_row "source"      "$($NEED_CLONE && echo "clone $REPO_URL${BRANCH:+ @$BRANCH} -> $SRC_DIR" || echo "$SRC_DIR   ${DIM}($SRC_ORIGIN)${NC}")"
  plan_row "rust"        "$rust_line"
  plan_row "build"       "$($DO_BUILD && echo "cargo build --release -j$JOBS   ${DIM}(default-members only — nothing GPL is linked)${NC}" || echo 'skipped (--no-build) — install the existing target/release build')"
  if $WITH_GUI; then
    plan_row "native GUI" "${YELLOW}BUILD IT${NC} — apexrouter-ui links Slint under its GPL option, so that"
    plan_row ""           "binary is GPL-3.0-only. Everything else stays MIT OR Apache-2.0."
    $GUI_LIBS_OK || plan_row "" "${YELLOW}fontconfig dev headers missing:${NC} $GUI_LIBS_HINT"
  else
    plan_row "native GUI" "skipped (the default) — the web UI is already inside the daemon"
  fi
  plan_row "service"     "$($WITH_SERVICE && echo "systemd --user unit -> $UNIT_FILE" || echo 'none — start it with: apexrouter serve --detach')"
  if [[ -n "$ENABLE_LINGER" ]]; then
    plan_row "linger"    "$($ENABLE_LINGER && echo "loginctl enable-linger $ME" || echo 'left as-is')"
  fi
  local path_plan
  case ":$PATH:" in
    *":$BIN_DIR:"*) path_plan="already has $BIN_DIR" ;;
    *) $MODIFY_PATH && path_plan="one marked line appended to your shell rc" \
                    || path_plan="unchanged — the export line is printed for you" ;;
  esac
  plan_row "PATH"        "$path_plan"
  plan_row "completions" "$($WITH_COMPLETIONS && echo 'bash / zsh / fish, best-effort' || echo 'skipped')"
  plan_row "llama.cpp"   "$( (( ${#LLAMA_FOUND[@]} )) && echo "${#LLAMA_FOUND[@]} build(s) discovered — none is ever compiled by this script" || echo 'none found — you get the options, not a 40-minute build')"
  # Every outbound call this script will make, named before it makes any of them.
  local cred_hosts=()
  if $CRED_WILL_CHECK; then
    [[ -n "$VAST_KEY" ]] && cred_hosts+=("console.vast.ai")
    [[ -n "$HF_KEY"   ]] && cred_hosts+=("huggingface.co")
    if [[ -n "$TOG_KEY" ]]; then
      local tog_host="${TOGETHER_BASE#*://}"; cred_hosts+=("${tog_host%%/*}")
    fi
  fi
  if (( ${#cred_hosts[@]} )); then
    plan_row "credentials" "offer the keys found above to ${cred_hosts[*]} — auth-only"
    plan_row ""            "reads that spend nothing. ${DIM}--offline skips them entirely.${NC}"
  else
    plan_row "credentials" "$($CRED_WILL_CHECK && echo 'none found — nothing is contacted' || echo 'not checked (--offline / --dry-run / no curl)')"
  fi
  plan_row "verify"      "$($SKIP_VERIFY && echo 'skipped' || echo "start it, confirm the serving pid is the binary just installed, run a verb, then GET /health")"
  echo ""
  note "Never touched: ~/.vastai-gguf (another tool's state directory), an existing"
  note "config.toml, and any credential you did not type in yourself."
  $CONF_RESTORED && note "Choices restored from $CONF_FILE${PREV_VERSION:+ (last installed v$PREV_VERSION)}"
  echo ""
  return 0
}

render_plan

if $DRY_RUN; then
  echo -e "${GREEN}${BOLD}  Dry run complete — nothing was written, no network was used.${NC}"
  echo -e "  ${DIM}Run without --dry-run to execute exactly the plan above.${NC}"
  echo ""
  exit 0
fi

if ! $YES; then
  BUILD_NOTE="No build this run (--no-build)."
  $DO_BUILD && BUILD_NOTE="Build time here: roughly $(( 6 + (24 / (JOBS < 1 ? 1 : JOBS)) )) minutes for a cold
release build of eight crates. A re-run reuses the target directory and is
much faster."
  tui_yesno "Proceed?" "Everything above, for real.

${BUILD_NOTE}

Live log: ${LOG:-none}
It is copied to ${STATE_DIR}/install.log when this finishes." yes \
    || { echo "Cancelled — nothing was changed."; exit 0; }
fi

# ══════════════════════════════════════════════════════════════════════════════
#  Execute
# ══════════════════════════════════════════════════════════════════════════════

START_TS=$(date +%s)

# ---- 0. the credential checks the plan just announced ------------------------
# First thing past the gate, and not one line before it: this is the only part of
# the run that talks to anybody, and the reader has now seen it listed by host.
step "checking the credentials that were found" \
     "one auth-only GET per key — nothing is created, modified or charged" \
     "pass --offline to skip every network call this script can make"
verify_credentials

# ---- 1. directories ----------------------------------------------------------
step "creating directories" \
     "mkdir -p $BIN_DIR $STATE_DIR" \
     "check the permissions on $PREFIX and \$HOME, or pass --prefix somewhere writable"
hdr "Directories"

SUDO=""
if $SYSTEM; then
  if [[ $EUID -ne 0 ]]; then
    have sudo || die "--system needs root and there is no sudo on this machine.
     Next: re-run as root, or drop --system for a user-level install in ~/.local."
    SUDO="sudo"
    info "system-wide install — sudo is used ONLY to copy binaries into $BIN_DIR"
    note "State, config, credentials and the service stay yours, in your home."
  fi
fi

mkdir -p "$STATE_DIR" || die "cannot create the state directory $STATE_DIR.
     It tried: mkdir -p $STATE_DIR as $(id -un).
     Next: pass --state-dir somewhere you can write."
chmod 700 "$STATE_DIR" 2>/dev/null || true
mkdir -p "$(dirname "$CONFIG_FILE")" || die "cannot create $(dirname "$CONFIG_FILE").
     Next: pass --state-dir, or set \$APEXROUTER_CONFIG to a writable path."
if [[ -n "$SUDO" ]]; then
  $SUDO mkdir -p "$BIN_DIR" || die "sudo could not create $BIN_DIR.
     It tried: sudo mkdir -p $BIN_DIR
     Next: drop --system for a user-level install in ~/.local — no root needed."
else
  mkdir -p "$BIN_DIR" 2>/dev/null \
    || die "cannot create $BIN_DIR.
     It tried: mkdir -p $BIN_DIR as $(id -un).
     Next: pass --prefix to somewhere you can write, or --system with sudo."
fi
ok "state:  $STATE_DIR"
ok "bin:    $BIN_DIR"

# ---- 2. source ---------------------------------------------------------------
if $NEED_CLONE; then
  step "fetching the source" \
       "git clone --depth=1 $REPO_URL $SRC_DIR" \
       "if the repository is private or not published yet, re-run with --from-source /path/to/ApexRouter-RS"
  hdr "Source"
  $OFFLINE && die "--offline was passed but there is no local checkout to build.
     Next: re-run with --from-source /path/to/ApexRouter-RS."
  have git || die "git is not installed and the source must be cloned.
     It tried: to run 'git clone $REPO_URL'.
     Next: install git (apt/dnf/pacman install git), or download a checkout and
     re-run with --from-source /path/to/ApexRouter-RS."
  mkdir -p "$(dirname "$SRC_DIR")"
  info "cloning $REPO_URL${BRANCH:+ (branch $BRANCH)} -> $SRC_DIR"
  CLONE_ARGS=(--depth=1)
  [[ -n "$BRANCH" ]] && CLONE_ARGS+=(--branch "$BRANCH")
  if ! git clone "${CLONE_ARGS[@]}" "$REPO_URL" "$SRC_DIR" 2>&1 | tail -3; then
    die "clone failed.
     It tried: git clone --depth=1 $REPO_URL $SRC_DIR
     Next: check the URL and your network. If the repository is not public yet,
     get a checkout by other means and re-run with --from-source <dir>."
  fi
  looks_like_repo "$SRC_DIR" || die "the clone succeeded but $SRC_DIR is not an ${PRODUCT} workspace.
     Next: check --repo-url."
  MSRV="$(_toml_val "$SRC_DIR/Cargo.toml" "workspace.package" "rust-version")"
  ok "cloned — MSRV $MSRV"
else
  hdr "Source"
  ok "$SRC_DIR  ($SRC_ORIGIN)"
  # A checkout we were pointed at is never pulled: it may be a working tree with
  # local changes, and silently moving someone's HEAD is not ours to do.
  if [[ -d "$SRC_DIR/.git" ]] && have git; then
    rev="$(git -C "$SRC_DIR" describe --tags --always --dirty 2>/dev/null || true)"
    [[ -n "$rev" ]] && note "at $rev (left exactly as it is — no pull, no checkout)"
  fi
fi

# ---- 3. Rust -----------------------------------------------------------------
if $DO_BUILD; then
  step "preparing the Rust toolchain" \
       "look for cargo on \$PATH and in ~/.cargo/bin" \
       "install Rust from https://rustup.rs and re-run, or pass --no-build to install a prebuilt binary"
  hdr "Rust toolchain"

  if [[ -z "$CARGO_BIN" ]]; then
    if ! $ALLOW_RUSTUP; then
      die "no Rust toolchain, and --no-rustup forbids installing one.
     Next: install Rust ($([[ -n "$MSRV" ]] && echo "$MSRV or newer" || echo 'a recent stable')) from https://rustup.rs, then re-run."
    fi
    $OFFLINE && die "no Rust toolchain and --offline forbids fetching one.
     Next: install Rust and re-run."
    have curl || die "no Rust toolchain and no curl to fetch rustup with.
     Next: install curl, or install Rust by hand from https://rustup.rs."
    info "installing rustup for $(id -un) — ~/.cargo and ~/.rustup, a few hundred MB"
    note "This is the standard Rust installer, unmodified, with --no-modify-path."
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal 2>&1 | tail -3 \
      || die "rustup install failed.
     It tried: curl https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal
     Next: install Rust by hand from https://rustup.rs and re-run."
    CARGO_BIN="$HOME/.cargo/bin/cargo"
    [[ -x "$CARGO_BIN" ]] || die "rustup finished but $CARGO_BIN is not there.
     Next: open a new shell, check 'cargo --version', then re-run."
    HAVE_RUSTUP=true
    CARGO_VER="$("$CARGO_BIN" --version 2>/dev/null | awk '{print $2}' || true)"
    ok "installed cargo $CARGO_VER"
  fi

  CARGO_DIR="$(dirname "$CARGO_BIN")"
  export PATH="$CARGO_DIR:$PATH"

  if [[ -n "$MSRV" && -n "$CARGO_VER" ]] && ! version_ge "$CARGO_VER" "$MSRV"; then
    warn "cargo $CARGO_VER is older than this workspace's MSRV ($MSRV, from Cargo.toml)"
    if $HAVE_RUSTUP && ! $OFFLINE; then
      info "running: rustup update stable"
      rustup update stable 2>&1 | tail -3 || true
      CARGO_VER="$("$CARGO_BIN" --version 2>/dev/null | awk '{print $2}' || true)"
    fi
    version_ge "$CARGO_VER" "$MSRV" || die "cargo is still $CARGO_VER, and this workspace needs $MSRV.
     It tried: rustup update stable.
     Next: upgrade Rust however your system provides it, then re-run.
     (The MSRV is declared in $SRC_DIR/Cargo.toml — it is read, not guessed.)"
  fi
  ok "cargo $CARGO_VER  ($CARGO_BIN)${MSRV:+  — MSRV $MSRV satisfied}"
fi

# ---- 4. build ----------------------------------------------------------------
BUILT_BIN="$SRC_DIR/target/release/$BIN_NAME"
BUILT_GUI="$SRC_DIR/target/release/$GUI_BIN_NAME"

# One filter for both builds: milestone lines a human can read, a crate counter
# for a log, and never a wall of output. The full log is on disk either way.
build_filter() {
  local n=0 name
  while IFS= read -r line; do
    case "$line" in
      *Compiling\ *)
        n=$(( n + 1 ))
        name="${line#*Compiling }"; name="${name%% *}"
        case "$name" in
          apexrouter-*|slint*) info "compiling $name" ;;
          *) (( n % 40 == 0 )) && note "... $n crates compiled" ;;
        esac ;;
      *Finished\ *) ok "cargo finished — ${line#*Finished }" ;;
      error*|*"error["*|*"error:"*) echo "$line" ;;
      warning:*unused*) : ;;
    esac
  done
  return 0
}

run_cargo() {                     # $@ = cargo args; echoes nothing, sets CARGO_RC
  local log="$1"; shift
  set +e
  ( cd "$SRC_DIR" && CARGO_BUILD_JOBS="$JOBS" "$CARGO_BIN" "$@" 2>&1 ) | tee -a "$log" | build_filter
  CARGO_RC=${PIPESTATUS[0]}
  set -e
}

if $DO_BUILD; then
  step "building the workspace" \
       "cargo build --release in $SRC_DIR with -j$JOBS" \
       "read the build log, then re-run; --jobs 1 if the kernel OOM-killed rustc (signal: 9)"
  hdr "Build (release, ${JOBS} job(s))"
  info "eight crates, ~120k lines of Rust. This is minutes, not seconds."
  note "Only default-members are built — the GPL Slint crate is not among them."
  BUILD_LOG="$STATE_DIR/install-build.log"
  : >"$BUILD_LOG" 2>/dev/null \
    || BUILD_LOG="$(mktemp -t apexrouter-build.XXXXXX.log 2>/dev/null || echo /tmp/apexrouter-build.log)"
  note "full build log: $BUILD_LOG"

  # --locked first: build exactly the committed Cargo.lock. A tree whose lock has
  # legitimately drifted (a fork, a local patch) retries without it, loudly.
  run_cargo "$BUILD_LOG" build --release --locked
  if (( CARGO_RC != 0 )) && grep -q -- "--locked" "$BUILD_LOG"; then
    warn "the committed Cargo.lock does not match this tree — retrying without --locked"
    run_cargo "$BUILD_LOG" build --release
  fi
  if (( CARGO_RC != 0 )); then
    grep -E "^error" "$BUILD_LOG" | head -8 || true
    if grep -q "signal: 9" "$BUILD_LOG"; then
      warn "rustc was OOM-killed (signal 9). ${JOBS} parallel jobs on $(( RAM_MB / 1024 )) GB was too many."
      note "Re-run with --jobs 1, or add swap. Nothing is broken — it just ran out of RAM."
    fi
    die "the build failed.
     It tried: cargo build --release -j$JOBS in $SRC_DIR
     Next: read $BUILD_LOG (the first 'error' line is the real one), fix it, re-run.
     Nothing has been installed — your existing setup is untouched."
  fi
  [[ -x "$BUILT_BIN" ]] || die "the build reported success but $BUILT_BIN is missing.
     Next: check $BUILD_LOG, and that CARGO_TARGET_DIR is not redirecting the output."
  ok "built $BIN_NAME  ($(human_bytes "$(stat -c %s "$BUILT_BIN" 2>/dev/null || echo 0)"))"

  if $WITH_GUI; then
    step "building the native GUI" \
         "cargo build --release -p apexrouter-slint" \
         "install the fontconfig development package, or re-run with --no-gui"
    hdr "Native GUI (GPL-3.0-only)"
    warn "apexrouter-ui links Slint under its GPL option: that binary is GPL-3.0-only."
    note "The daemon, proxy, web UI, CLI and MCP server stay MIT OR Apache-2.0."
    note "Distributing apexrouter-ui takes on GPL obligations for it. docs/LICENSING.md."
    if ! $GUI_LIBS_OK; then
      warn "fontconfig development headers were not found — the build may fail."
      note "Install them with: $GUI_LIBS_HINT"
    fi
    run_cargo "$BUILD_LOG" build --release -p apexrouter-slint
    if (( CARGO_RC != 0 )); then
      grep -E "^error" "$BUILD_LOG" | head -6 || true
      warn "the GUI build failed — continuing without it (the daemon is unaffected)."
      note "Most likely the system dev packages: $GUI_LIBS_HINT"
      note "Then re-run this installer with --with-gui."
      WITH_GUI=false
    else
      if [[ -x "$BUILT_GUI" ]]; then
        ok "built $GUI_BIN_NAME  ($(human_bytes "$(stat -c %s "$BUILT_GUI" 2>/dev/null || echo 0)"))"
      else
        WITH_GUI=false
      fi
    fi
  fi
else
  step "locating a prebuilt binary" \
       "look for $BUILT_BIN, because --no-build says not to compile one" \
       "drop --no-build, or run 'cargo build --release' in the source tree first"
  hdr "Build"
  info "--no-build: using the existing $BUILT_BIN"
  [[ -x "$BUILT_BIN" ]] || die "--no-build was passed but $BUILT_BIN does not exist.
     Next: drop --no-build, or build it yourself with 'cargo build --release'."
  $WITH_GUI && { [[ -x "$BUILT_GUI" ]] || { warn "no prebuilt $GUI_BIN_NAME — skipping the GUI"; WITH_GUI=false; }; }
fi

# ---- 5. install binaries -----------------------------------------------------
step "installing the binaries" \
     "copy into $BIN_DIR via a temp file and rename" \
     "check that $BIN_DIR is writable, or choose another --prefix"
hdr "Installing"

# Rename, never overwrite-in-place: a running daemon holds its inode, and writing
# over a live executable is ETXTBSY ("Text file busy"). Rename is atomic and the
# running process keeps the old inode until it restarts — which is exactly what
# lets an upgrade happen under a daemon that is currently serving.
install_bin() {
  local src="$1" dest="$2" tmp="$2.new.$$"
  if [[ -n "$SUDO" ]]; then
    $SUDO install -m 755 "$src" "$tmp" && $SUDO mv -f "$tmp" "$dest"
  else
    install -m 755 "$src" "$tmp" && mv -f "$tmp" "$dest"
  fi || die "could not install $dest.
     It tried: install -m 755 $src $tmp && mv -f $tmp $dest
     Next: check permissions on $BIN_DIR, or pass a --prefix you own."
  ok "$(basename "$dest") -> $dest  ($(human_bytes "$(stat -c %s "$dest" 2>/dev/null || echo 0)"))"
}

install_bin "$BUILT_BIN" "$BIN_DIR/$BIN_NAME"
$WITH_GUI && install_bin "$BUILT_GUI" "$BIN_DIR/$GUI_BIN_NAME"

APEX="$BIN_DIR/$BIN_NAME"
NEW_VER="$("$APEX" --version 2>/dev/null | awk '{print $2}' || echo unknown)"
[[ -n "$PREV_VERSION" && "$PREV_VERSION" != "$NEW_VER" ]] \
  && info "upgraded v$PREV_VERSION -> v$NEW_VER"

# ---- 6. config ---------------------------------------------------------------
step "seeding the configuration" \
     "apexrouter config init (only when no config.toml exists)" \
     "run 'apexrouter config path' to see where it is looking"
hdr "Configuration"

# Only export it when it is not the default — see resolve_config_path().
$STATE_IS_DEFAULT || export APEXROUTER_HOME="$STATE_DIR"
if [[ -f "$CONFIG_FILE" ]]; then
  ok "kept your existing $CONFIG_FILE (never overwritten)"
  note "Zero config is a working install; every field is defaulted and commented."
elif $DAEMON_RUNNING; then
  warn "a daemon is running and holds the state lock — skipping config seeding"
  note "Stop it and run: apexrouter config init"
else
  if APEXROUTER_CONFIG="$CONFIG_FILE" "$APEX" config init >/dev/null 2>&1; then
    ok "wrote $CONFIG_FILE (the fully commented example — every field defaulted)"
  else
    warn "could not seed $CONFIG_FILE — that is fine, the defaults are complete"
    note "Create it later with: apexrouter config init"
  fi
fi

# llama.cpp guidance. We never compile it: the flags are hardware-specific and
# the wrong ones cost an hour and produce a CPU-only build that looks fine.
if (( ${#LLAMA_FOUND[@]} == 0 )); then
  hdr "llama.cpp — not found"
  echo "  ApexRouter supervises llama-server; it does not build it, because the"
  echo "  backend flags are yours to choose and a wrong guess costs you an hour"
  echo "  and produces a CPU-only binary that looks like it worked."
  echo ""
  echo "  Your options, cheapest first:"
  echo "    1. A managed provider. No local GPU needed:"
  echo "         export TOGETHER_API_KEY=...   then   apexrouter switch together"
  echo "    2. A LAN box that already runs llama-server or vLLM:"
  echo "         apexrouter backend add http://box:8080/v1 --label box"
  echo "    3. Rent a GPU by the hour (nothing spends without an approval):"
  echo "         apexrouter vast offers --gpu 'RTX 4090' --max-price 0.40"
  echo "    4. Build llama.cpp yourself, with the backend your card wants:"
  echo "         git clone https://github.com/ggml-org/llama.cpp ~/llama.cpp"
  echo "         cmake -B build-vulkan -DGGML_VULKAN=ON   # or -DGGML_CUDA=ON, -DGGML_HIP=ON"
  echo "         cmake --build build-vulkan -j"
  echo "       Then 'apexrouter rig' finds it: build*/bin/llama-server under"
  echo "       ~/llama.cpp, ~/Projects/llama.cpp or /usr/local/bin."
  echo ""
  note "Do not infer capability from a directory name — 'apexrouter rig' reports"
  note "what each build's own --list-devices says, which is the only honest source."
else
  info "${#LLAMA_FOUND[@]} llama.cpp build(s) will be discovered by 'apexrouter rig'"
fi

# ---- 7. shell completions ----------------------------------------------------
# Completions follow the PREFIX, not $HOME: with the default ~/.local prefix
# these land exactly where XDG says they should, and with another prefix they
# stay inside it, so two installs never fight over one file.
#
# fish is the exception and goes to $XDG_CONFIG_HOME/fish/completions, which is
# fish's own documented user location — and, more to the point, is where the
# repository's uninstall.sh looks for it. These two files must agree about every
# path or an uninstall leaves litter; if you move one, move the other.
COMP_BASH="$PREFIX/share/bash-completion/completions/$BIN_NAME"
COMP_ZSH="$PREFIX/share/zsh/site-functions/_$BIN_NAME"
COMP_FISH="${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions/$BIN_NAME.fish"

if $WITH_COMPLETIONS; then
  step "writing shell completions" "apexrouter completions <shell>" "harmless to skip: --no-completions"
  hdr "Shell completions"
  gen_completion() {                # $1=shell $2=dest path
    local dir; dir="$(dirname "$2")"
    if [[ -n "$SUDO" ]]; then
      $SUDO mkdir -p "$dir" 2>/dev/null || return 1
      "$APEX" completions "$1" 2>/dev/null | $SUDO tee "$2" >/dev/null || return 1
    else
      mkdir -p "$dir" 2>/dev/null || return 1
      "$APEX" completions "$1" >"$2" 2>/dev/null || { rm -f "$2"; return 1; }
    fi
    ok "$1 -> $2"
  }
  gen_completion bash "$COMP_BASH" || true
  gen_completion zsh  "$COMP_ZSH"  || true
  gen_completion fish "$COMP_FISH" || true
  note "zsh needs $PREFIX/share/zsh/site-functions on \$fpath;"
  note "bash needs the bash-completion package, and \$XDG_DATA_DIRS to include"
  note "$PREFIX/share if the prefix is not ~/.local."
fi

# ---- 8. PATH -----------------------------------------------------------------
PATH_LINE="export PATH=\"$BIN_DIR:\$PATH\""
PATH_NEEDS_FIX=false
case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;
  *) PATH_NEEDS_FIX=true ;;
esac

if $PATH_NEEDS_FIX; then
  step "putting $BIN_DIR on your PATH" "append one marked line to your shell rc" "paste the export line yourself: --no-modify-path"
  hdr "PATH"
  if $MODIFY_PATH; then
    RC=""
    case "$(basename "${SHELL:-/bin/bash}")" in
      zsh)  RC="${ZDOTDIR:-$HOME}/.zshrc" ;;
      fish) RC="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish" ;;
      *)    RC="$HOME/.bashrc" ;;
    esac
    MARKER="# added by ${PRODUCT} install.sh"
    # Decide the line BEFORE opening the file for append: a `{ … } >>"$RC"`
    # block that also reads $RC inside itself is a shape that is hard to read and
    # that shellcheck rightly distrusts (SC2094).
    PATH_APPEND="$PATH_LINE"
    [[ "$RC" == *fish* ]] && PATH_APPEND="set -gx PATH $BIN_DIR \$PATH"

    # What is checked is whether the rc already exports THIS bin dir — not
    # whether it carries the marker. The marker names no prefix, so looking for
    # it meant a second install to a different --prefix found somebody else's
    # line, announced "already carries the PATH line", and added nothing: the rc
    # went on exporting the first prefix forever while the installer called it
    # done. Same defect class as the port check — matching on a landmark that is
    # not the thing you are actually asking about.
    HAD_OTHER_MARKER=false
    if [[ -f "$RC" ]] && grep -Fq "$MARKER" "$RC"; then HAD_OTHER_MARKER=true; fi

    if [[ -f "$RC" ]] && grep -Fqx "$PATH_APPEND" "$RC"; then
      ok "$RC already puts $BIN_DIR on your PATH"
    else
      mkdir -p "$(dirname "$RC")"
      if printf '\n%s  (%s)\n%s\n' "$MARKER" "$BIN_DIR" "$PATH_APPEND" >>"$RC"; then
        ok "appended the PATH line to $RC"
        if $HAD_OTHER_MARKER; then
          warn "$RC also carries an earlier ${PRODUCT} PATH line for a different prefix."
          note "Both are now exported. Delete the one you do not want — grep for:"
          note "  $MARKER"
        fi
      else
        warn "could not write $RC — paste this yourself: $PATH_LINE"
      fi
      note "Open a new shell, or run: $PATH_LINE"
    fi
  else
    warn "$BIN_DIR is not on your \$PATH, and no shell rc will be touched."
    note "Paste this into your shell rc:  $PATH_LINE"
  fi
fi

# ---- 9. systemd --user service ----------------------------------------------
SERVICE_ACTIVE=false
if $WITH_SERVICE; then
  step "installing the systemd --user unit" \
       "write $UNIT_FILE, then systemctl --user daemon-reload and enable" \
       "run it by hand instead: $APEX serve --detach"
  hdr "Service (systemd --user — no root)"

  mkdir -p "$UNIT_DIR"
  UNIT_TMP="$UNIT_FILE.new.$$"
  # APEXROUTER_HOME is written ONLY for a non-default state dir: setting it also
  # moves where config.toml is read from (core/src/paths.rs), so pinning it to
  # the value that is already the default would silently orphan an existing
  # ~/.config/apexrouter/config.toml.
  UNIT_ENV_HOME=""
  $STATE_IS_DEFAULT || UNIT_ENV_HOME="Environment=APEXROUTER_HOME=${STATE_DIR}"
  cat >"$UNIT_TMP" <<UNIT
[Unit]
Description=${PRODUCT} — one base URL for every model you can reach
Documentation=${DEFAULT_REPO_URL}
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
${UNIT_ENV_HOME}
Environment=RUST_LOG=info
ExecStart=${APEX} serve --foreground
Restart=on-failure
RestartSec=3

# llama-server children are spawned with setsid() and are MEANT to outlive the
# manager: restart the daemon, upgrade the binary, crash it — the model that took
# 90 seconds and 6 GB to load is still there and gets re-adopted by
# (pid, start_time_ticks, boot_id, exe). setsid() does NOT escape the cgroup, so
# the default KillMode=control-group would SIGKILL exactly the thing that design
# exists to protect. KillMode=process signals only the daemon.
KillMode=process
KillSignal=SIGTERM
# [server] drain_timeout_secs defaults to 30 — in-flight requests get to finish.
TimeoutStopSec=45

[Install]
WantedBy=default.target
UNIT

  if [[ -f "$UNIT_FILE" ]] && ! cmp -s "$UNIT_TMP" "$UNIT_FILE"; then
    cp -f "$UNIT_FILE" "$UNIT_FILE.bak.$(date +%Y%m%d%H%M%S)"
    info "your previous unit differed — backed it up next to the new one"
  fi
  mv -f "$UNIT_TMP" "$UNIT_FILE"
  ok "wrote $UNIT_FILE"

  systemctl --user daemon-reload || warn "systemctl --user daemon-reload failed"
  if systemctl --user enable "$SERVICE_NAME" >/dev/null 2>&1; then
    ok "enabled ${SERVICE_NAME}.service (starts with your session)"
  else
    warn "could not enable the unit — start it by hand: systemctl --user start $SERVICE_NAME"
  fi

  if [[ -z "$ENABLE_LINGER" ]] && $YES && ! $LINGER_ON; then
    # Unattended, and a service was asked for: a daemon that dies at logout is
    # not a daemon. Reversible, announced, and refusable with --no-linger.
    ENABLE_LINGER=true
  fi
  if [[ "${ENABLE_LINGER:-false}" == true ]] && ! $LINGER_ON && have loginctl; then
    info "enabling linger so the daemon survives logout (undo: loginctl disable-linger $ME)"
    if loginctl enable-linger "$ME" >/dev/null 2>&1; then
      ok "linger enabled — the daemon survives logout (undo: loginctl disable-linger $ME)"
      LINGER_ON=true
    else
      warn "could not enable linger (needs polkit or root)"
      note "Without it systemd stops your user services at logout. Fix later with:"
      note "  sudo loginctl enable-linger $ME"
    fi
  fi
else
  hdr "Service"
  info "no service installed."
  note "Start it yourself:  $APEX serve --detach      (returns once /health answers)"
  note "Or in the foreground: $APEX serve --foreground"
  note "Any mutating verb autostarts one anyway ([server] autostart = true)."
fi

# ---- 10. remember the choices ------------------------------------------------
step "saving the resolved choices" "write $CONF_FILE" "delete that file to start from auto-detect again"
write_install_conf() {
  local tmp="$CONF_FILE.new.$$"
  {
    echo "# ${PRODUCT} — resolved install choices, written by install.sh."
    echo "# Re-running the installer restores these, so a re-run UPGRADES the install"
    echo "# instead of re-deciding it. Precedence: CLI flag > this file > auto-detect."
    echo "# Delete this file to go back to pure auto-detection."
    echo "APEXROUTER_INSTALL_PREFIX=$PREFIX"
    echo "APEXROUTER_INSTALL_SYSTEM=$SYSTEM"
    echo "APEXROUTER_INSTALL_GUI=$WITH_GUI"
    echo "APEXROUTER_INSTALL_SERVICE=$WITH_SERVICE"
    echo "APEXROUTER_INSTALL_SOURCE=$SRC_DIR"
    echo "APEXROUTER_INSTALL_REPO=$REPO_URL"
    echo "APEXROUTER_INSTALL_JOBS=$JOBS"
    echo "APEXROUTER_INSTALL_MODIFY_PATH=$MODIFY_PATH"
    echo "APEXROUTER_INSTALL_VERSION=$NEW_VER"
    echo "APEXROUTER_INSTALL_DATE=$(date -Is)"
    echo "APEXROUTER_INSTALL_STATE=$STATE_DIR"
  } >"$tmp"
  chmod 600 "$tmp"
  mv -f "$tmp" "$CONF_FILE"
}
if write_install_conf; then
  ok "choices saved -> $CONF_FILE"
else
  warn "could not write $CONF_FILE — a re-run will re-detect instead of restoring"
fi

# ══════════════════════════════════════════════════════════════════════════════
#  Verify — that THIS install is what is now serving, proved on this machine
# ══════════════════════════════════════════════════════════════════════════════
#
# The people who install this already run inference stacks, so 8888 and 2739 are
# ports somebody else's process may well be holding. "Something answers on 8888"
# is therefore not a verification — it is a coincidence that reads exactly like
# one, and it is how this script once printed six green checks and
# "ApexRouter-RS installed" while the only daemon on the box belonged to a
# different install and every verb of the one just written was dead.
#
# So nothing here is anchored to a port. Everything is anchored to the binary
# this run put on disk:
#
#   1. that binary must be able to read the config it will run on (its stderr is
#      shown, not swallowed — an unparseable config kills every verb at once);
#   2. the exit status of whatever started the daemon is KEPT. `serve --detach`
#      exits 1 and names the port's holder; `|| true` threw that away;
#   3. the process that is serving is identified from the owner record the daemon
#      writes into $STATE/apexrouterd.lock, and that pid's /proc/<pid>/exe must be
#      the inode we just installed;
#   4. one real verb runs through the installed binary and must reach the daemon.
#
# Only once those hold does an HTTP answer on those ports mean anything, so the
# HTTP checks run last and are skipped — never counted — when they cannot.

PROXY_BASE="http://127.0.0.1:${DEFAULT_PROXY_PORT}"
CONTROL_BASE="http://127.0.0.1:${DEFAULT_CONTROL_PORT}"
VERIFY_PASS=0; VERIFY_FAIL=0

check() {  # $1=label $2=pass|<reason>
  if [[ "$2" == "pass" ]]; then ok "$1"; VERIFY_PASS=$((VERIFY_PASS+1))
  else warn "$1 — $2"; VERIFY_FAIL=$((VERIFY_FAIL+1)); fi
}

# A check that was not run, because an earlier one made it meaningless. Neither a
# pass nor a fail: the real failure is already counted, and inventing a second one
# per skipped row would bury it.
skip() { echo -e "${DIM}  -${NC} $1  ${DIM}(not run: $2)${NC}"; }

wait_http() {  # $1=url $2=seconds
  local n=0
  while (( n < $2 )); do
    curl -fsS --max-time 2 -o /dev/null "$1" 2>/dev/null && return 0
    sleep 1; n=$((n+1))
  done
  return 1
}

# The first meaningful line of a captured stderr, folded to fit one check row.
# The whole of it is in the install log regardless — this is the headline, not
# the report, and a check that says "it failed" without saying how is the thing
# this rewrite exists to delete.
VERIFY_ERR=""
err_line() {
  [[ -n "$VERIFY_ERR" && -s "$VERIFY_ERR" ]] || { printf 'no output on stderr'; return 0; }
  grep -v '^[[:space:]]*$' "$VERIFY_ERR" 2>/dev/null | head -1 | cut -c1-150 || true
}

# …and the `--json` surfaces do not use stderr at all: they print an ErrorEnvelope
# on STDOUT and exit 1 (apexrouter-protocol). Looking for their reason on stderr
# finds an empty file and reports "no output", which is how a verification message
# ends up less informative than the thing it is describing. $1 = captured stdout.
json_err() {
  local m; m="$(json_field "$1" message)"
  [[ -n "$m" ]] || return 1
  m="${m//\\n/ }"      # a multi-line TOML error still has to fit on one row
  printf '%s' "$m" | tr -s ' ' | cut -c1-180
}

# ── Who is actually serving out of $STATE_DIR, and is it what we just installed?
#
# The daemon writes an owner record — pid, start_time_ticks, boot_id, version,
# proxy_url, control_url — into $STATE/apexrouterd.lock the instant it takes the
# lock (core/src/lockfile.rs). That file is evidence, not proof: it is left on
# disk by a daemon that took the lock, failed to bind and exited, which is
# precisely one of the shapes here. So the pid is checked for life, and then for
# identity.
#
# Identity is the exe INODE, not the path. install_bin renames a new inode over
# the old one, so a daemon that did not really restart keeps running the unlinked
# old file and reads as `/path/to/apexrouter (deleted)` — that is the broken-config
# upgrade, where `serve --stop` fails, nothing restarts, and the old process goes
# on answering /health with a config no verb can parse any more. A daemon from
# somebody else's prefix resolves to somebody else's inode. Both are caught here,
# and they are reported differently, because the fix differs.
#
# Sets DAEMON_ID (ours|foreign|stale-exe|dead|norecord|unreadable), DAEMON_PID,
# DAEMON_EXE and DAEMON_WHY (one sentence, for a human, naming the next move).
DAEMON_ID="norecord"; DAEMON_PID=""; DAEMON_EXE=""; DAEMON_WHY=""
daemon_identity() {
  DAEMON_ID="norecord"; DAEMON_PID=""; DAEMON_EXE=""; DAEMON_WHY=""
  local rec want have
  rec="$(cat "$LOCK_FILE" 2>/dev/null || true)"
  DAEMON_PID="$(json_field "$rec" pid)"

  if [[ -z "$DAEMON_PID" ]]; then
    DAEMON_WHY="nothing has taken the daemon lock in this state dir — $LOCK_FILE holds no owner record"
    return 0
  fi
  # /proc, not `kill -0`: kill reports EPERM for a live process owned by another
  # user, and calling that "dead" would turn a foreign daemon into a mystery.
  if [[ ! -d "/proc/$DAEMON_PID" ]]; then
    DAEMON_ID="dead"
    DAEMON_WHY="the owner record names pid $DAEMON_PID, and that process is gone — it took the lock, failed to start, and exited"
    return 0
  fi
  DAEMON_EXE="$(readlink "/proc/$DAEMON_PID/exe" 2>/dev/null || true)"
  if [[ -z "$DAEMON_EXE" ]]; then
    DAEMON_ID="unreadable"
    DAEMON_WHY="pid $DAEMON_PID is alive but /proc/$DAEMON_PID/exe cannot be read (it runs as $(ps -o user= -p "$DAEMON_PID" 2>/dev/null | tr -d ' ' || echo 'another user')), so it is not this install's daemon"
    return 0
  fi
  if [[ "$DAEMON_EXE" == *" (deleted)" ]]; then
    DAEMON_ID="stale-exe"
    DAEMON_WHY="pid $DAEMON_PID is still running the OLD, now-unlinked binary — the restart never happened, so the version answering is not the one just installed"
    return 0
  fi
  want="$(stat -c '%d:%i' "$APEX" 2>/dev/null || true)"
  have="$(stat -L -c '%d:%i' "/proc/$DAEMON_PID/exe" 2>/dev/null || true)"
  if [[ -n "$want" && -n "$have" && "$want" == "$have" ]]; then
    DAEMON_ID="ours"
    return 0
  fi
  DAEMON_ID="foreign"
  if [[ "$DAEMON_EXE" == "$APEX" ]]; then
    DAEMON_WHY="pid $DAEMON_PID runs $DAEMON_EXE, but a different build than the file now at that path"
  else
    DAEMON_WHY="pid $DAEMON_PID runs $DAEMON_EXE — that is another install, not $APEX"
  fi
  return 0
}

if ! $SKIP_VERIFY && ! have curl; then
  hdr "Verify"
  warn "no curl on this machine, so the health checks cannot run."
  note "Everything is installed. Check it yourself with:  $BIN_DIR/$BIN_NAME status"
  SKIP_VERIFY=true
fi

if ! $SKIP_VERIFY; then
  step "verifying that what was installed is what is now serving" \
       "parse the config, start the daemon, confirm the serving pid is $APEX, run a verb, then GET /health" \
       "read the daemon log: $STATE_DIR/logs/apexrouterd.log — and 'apexrouter doctor' names one fix per row"
  hdr "Verify"

  VERIFY_ERR="$(mktemp -t apexrouter-verify.XXXXXX 2>/dev/null || echo "/tmp/apexrouter-verify.$$")"

  # ---- 1. can the installed binary read the config it will run on? ------------
  # This also answers "where will it listen", so the defaults are never assumed.
  # Its stderr used to go to /dev/null behind `|| true`: a config.toml with a
  # duplicate table made every verb exit 1 with a TOML parse error, and the
  # installer, holding that exact error in its hand, threw it away and reported
  # six passes.
  CONFIG_OK=true
  cfg_json=""
  if cfg_json="$("$APEX" config show --json 2>"$VERIFY_ERR")"; then
    check "config         $CONFIG_FILE parses" pass
  else
    CONFIG_OK=false
    check "config         $CONFIG_FILE" "$(json_err "$cfg_json" || err_line)"
    cfg_json=""
    note "Every apexrouter verb loads this file before it does anything, so this"
    note "breaks all of them at once — including the ones that start the daemon."
    note "Fix it, or move it aside and run:  $BIN_DIR/$BIN_NAME config init"
  fi
  pb="$(json_field "$cfg_json" proxy_bind)"
  cb="$(json_field "$cfg_json" control_bind)"
  [[ -n "$pb" ]] && PROXY_BASE="http://${pb/0.0.0.0/127.0.0.1}"
  [[ -n "$cb" ]] && CONTROL_BASE="http://${cb/0.0.0.0/127.0.0.1}"

  # ---- 2. start it, and keep the exit status ----------------------------------
  START_RC=0
  STARTER=""
  if $WITH_SERVICE; then
    info "starting ${SERVICE_NAME}.service"
    STARTER="systemctl --user restart $SERVICE_NAME"
    systemctl --user restart "$SERVICE_NAME" >"$VERIFY_ERR" 2>&1 || START_RC=$?
  elif $DAEMON_RUNNING; then
    # A daemon was already up when we started, and its binary has just been
    # replaced underneath it (by rename — it is still running the old inode).
    # Restart it so the version you installed is the version that is serving.
    # Your llama-server children are NOT affected: they were spawned with
    # setsid() to outlive the manager, and the new daemon re-adopts them by
    # (pid, start_time_ticks, boot_id, exe).
    info "restarting the running daemon onto the new binary"
    note "loaded models keep running and are re-adopted — that is the design."
    STARTER="$BIN_NAME serve --stop, then $BIN_NAME serve --detach"
    # A --stop that fails is evidence, not noise. With a config the parser
    # rejects it is the FIRST thing to fail, the old daemon is never asked to
    # leave, and everything after this is measuring the process we meant to
    # replace. Say so at the moment it happens.
    if ! "$APEX" serve --stop >"$VERIFY_ERR" 2>&1; then
      warn "'$BIN_NAME serve --stop' failed: $(err_line)"
      note "The running daemon was never asked to stop, so it is still on the old"
      note "binary and still holding the ports. The identity check below says so."
    fi
    sleep 1
    "$APEX" serve --detach >"$VERIFY_ERR" 2>&1 || START_RC=$?
  else
    info "starting the daemon in the background ($BIN_NAME serve --detach)"
    STARTER="$BIN_NAME serve --detach"
    "$APEX" serve --detach >"$VERIFY_ERR" 2>&1 || START_RC=$?
  fi

  if (( START_RC == 0 )); then
    check "daemon start   $STARTER" pass
  else
    check "daemon start   $STARTER" "exit $START_RC: $(err_line)"
    note "Daemon log: $STATE_DIR/logs/apexrouterd.log"
    if $WITH_SERVICE; then
      note "Look at:  systemctl --user status $SERVICE_NAME"
      note "          journalctl --user -u $SERVICE_NAME -n 40 --no-pager"
    else
      note "Look at:  $APEX serve --foreground   (it says why, on stderr)"
    fi
  fi

  # ---- 3. is the thing that is serving the thing we installed? ----------------
  # The waiting happens here and is deliberately NOT a check: a socket answering
  # proves a socket is open, and on this audience's machines that socket is often
  # somebody else's. It buys the daemon its startup time, nothing more.
  CONTROL_ANSWERS=false
  if $CONFIG_OK || $DAEMON_RUNNING; then
    wait_http "$CONTROL_BASE/health" 30 && CONTROL_ANSWERS=true
  else
    # Every verb loads the config, including the one that starts the daemon, so
    # nothing of ours can possibly be listening. Waiting thirty seconds to be
    # told that a second time is just a slower way of saying it.
    note "not waiting on /health — the config above did not parse, so nothing started"
  fi

  daemon_identity
  if [[ "$DAEMON_ID" == "ours" ]]; then
    check "daemon         pid $DAEMON_PID is $APEX" pass
  else
    check "daemon         the process serving is the one just installed" "$DAEMON_WHY"
    # Whoever does hold the ports is the single most useful fact at this point.
    for _pp in "$DEFAULT_PROXY_PORT" "$DEFAULT_CONTROL_PORT"; do
      _who="$(port_holder "$_pp" || true)"
      [[ -n "$_who" ]] && note "port $_pp is held by: $_who"
    done
    unset _pp _who
    case "$DAEMON_ID" in
      dead|norecord)
        note "Nothing from this install is running. The usual cause is a port that"
        note "was already busy: free it, or set [server] proxy_bind / control_bind"
        note "in $CONFIG_FILE and re-run." ;;
      stale-exe)
        note "Stop the old process and start the new one:"
        note "  $APEX serve --stop  &&  $APEX serve --detach"
        note "If --stop refuses, fix what it complains about first: it loads the"
        note "config too, so a config it cannot parse stops the upgrade right here." ;;
      foreign|unreadable)
        note "Two installs cannot share one pair of ports. Either stop that daemon,"
        note "or give this one its own: [server] proxy_bind / control_bind in"
        note "$CONFIG_FILE." ;;
    esac
  fi

  # ---- 4. a verb, through the installed binary, reaching the daemon -----------
  # Not a port: the binary. `status --json` loads the config, finds the daemon
  # from its owner record and asks it — so `served_by: daemon` is the CLI, the
  # config, the state dir and the daemon agreeing with each other in one call.
  if [[ "$DAEMON_ID" == "ours" ]]; then
    if st_json="$("$APEX" status --json 2>"$VERIFY_ERR")"; then
      served="$(json_field "$st_json" served_by)"
      if [[ "$served" == "daemon" ]]; then
        check "CLI            $BIN_NAME status --json  (served_by: daemon)" pass
      else
        check "CLI            $BIN_NAME status --json" "answered from disk (served_by: ${served:-unknown}), not by the daemon"
      fi
    else
      # Same shape as `config show --json`: the reason is on stdout, not stderr.
      check "CLI            $BIN_NAME status --json" "$(json_err "$st_json" || err_line)"
    fi
  else
    skip "CLI            $BIN_NAME status --json" "the daemon is not this install's"
  fi

  # ---- 5. only now do the ports mean anything ---------------------------------
  if [[ "$DAEMON_ID" == "ours" ]] && $CONTROL_ANSWERS; then
    check "control plane  $CONTROL_BASE/health" pass
    SERVICE_ACTIVE=true
  elif [[ "$DAEMON_ID" == "ours" ]]; then
    check "control plane  $CONTROL_BASE/health" "no answer after 30s"
  else
    skip "control plane  $CONTROL_BASE/health" "an answer here would not be ours"
  fi

  if $SERVICE_ACTIVE; then
    if wait_http "$PROXY_BASE/health" 10; then
      check "proxy          $PROXY_BASE/health" pass
    else
      check "proxy          $PROXY_BASE/health" "no answer"
    fi

    # The whole product promise: BOTH base-URL forms reach the same route. A
    # client whose base is $PROXY_BASE asks for /v1/models; a client whose base
    # is $PROXY_BASE/v1 asks for /v1/v1/models. Both must be 200.
    if curl -fsS --max-time 8 -o /dev/null "$PROXY_BASE/v1/models" 2>/dev/null; then
      check "base URL       $PROXY_BASE/v1  (GET /v1/models)" pass
    else
      check "base URL       $PROXY_BASE/v1" "GET /v1/models did not answer"
    fi
    if curl -fsS --max-time 8 -o /dev/null "$PROXY_BASE/v1/v1/models" 2>/dev/null; then
      check "base URL       $PROXY_BASE      (a doubled /v1 is collapsed)" pass
    else
      check "base URL       $PROXY_BASE" "a doubled /v1 was not collapsed"
    fi
    if curl -fsS --max-time 8 -o /dev/null "$PROXY_BASE/models" 2>/dev/null; then
      check "base URL       $PROXY_BASE      (a missing /v1 is added)" pass
    else
      check "base URL       $PROXY_BASE" "a missing /v1 was not added"
    fi

    if curl -fsS --max-time 5 -o /dev/null "$CONTROL_BASE/" 2>/dev/null; then
      check "web UI         $CONTROL_BASE/" pass
    else
      check "web UI         $CONTROL_BASE/" "not served"
    fi
  else
    skip "proxy, base URLs, web UI" "there is no daemon of ours to ask"
  fi

  rm -f "$VERIFY_ERR" 2>/dev/null || true
fi

# Interactive tail: offer to store a key the user types. The BINARY is the only
# thing that ever writes a credential (0600, atomic) — the installer never does.
if ! $YES && $SERVICE_ACTIVE && [[ -z "$TOG_KEY" ]]; then
  if tui_yesno "Add a managed provider key?" \
"No together.ai key was found. You do not need one — local models and rented
GPUs work without it — but it is the fastest way to have a model behind
'auto' on a machine with no GPU at all.

If you say yes, the key is read from a prompt and handed to
  apexrouter provider set together --key-stdin
which writes it to $STATE_DIR/credentials.toml at 0600. The installer never
touches it. Leave blank to skip." no; then
    k=$(tui_input "together.ai API key" "Paste the key (it is not echoed):" "" password)
    if [[ -n "$k" ]]; then
      if printf '%s' "$k" | "$APEX" provider set together --key-stdin >/dev/null 2>&1; then
        ok "stored — 'apexrouter provider ls' shows the source, never the value"
      else
        warn "could not store it. Do it later with: apexrouter provider set together --key-stdin"
      fi
      unset k
    fi
  fi
fi

# ══════════════════════════════════════════════════════════════════════════════
#  Done
# ══════════════════════════════════════════════════════════════════════════════

ELAPSED=$(( $(date +%s) - START_TS ))
echo ""
# The headline is a claim, and it is only made when it is true. The files may all
# be on disk and the product still not be serving; saying "installed" in that case
# is the lie this whole section exists to stop telling, because the reader is
# usually `curl … | bash && echo ok`, a CI job, or an agent — none of which read
# prose, and all of which read $?.
if (( VERIFY_FAIL > 0 )); then
  VERIFY_FAILED=true
  echo -e "${YELLOW}${BOLD}  ${PRODUCT} v${NEW_VER} is on disk, but ${VERIFY_FAIL} check(s) failed${NC}  ${DIM}(${ELAPSED}s)${NC}"
  echo -e "  ${DIM}Verify: ${VERIFY_PASS} passed, ${VERIFY_FAIL} failed — the lines above name each one.${NC}"
  echo -e "  ${DIM}The binaries are installed and re-running this script is safe.${NC}"
else
  echo -e "${GREEN}${BOLD}  ${PRODUCT} v${NEW_VER} installed${NC}  ${DIM}(${ELAPSED}s)${NC}"
  if ! $SKIP_VERIFY; then
    echo -e "  ${DIM}Verify: ${VERIFY_PASS} passed${NC}"
  fi
fi
echo ""
if (( VERIFY_FAIL == 0 )); then
  echo -e "  ${BOLD}Point everything here, and never change it again:${NC}"
else
  echo -e "  ${BOLD}Once it is serving, this is the one URL:${NC}"
fi
echo ""
echo -e "      ${BOLD}${PROXY_BASE}/v1${NC}"
echo ""
echo -e "  ${DIM}Both forms work as a base URL: ${PROXY_BASE} and ${PROXY_BASE}/v1.${NC}"
echo -e "  ${DIM}A missing /v1 is added, a repeated one collapsed — so an SDK that appends${NC}"
echo -e "  ${DIM}/v1 for you and a script that already has it both hit the same route.${NC}"
echo ""
echo -e "  ${BOLD}Paste this into your shell:${NC}"
echo ""
if [[ -x "$APEX" ]]; then
  "$APEX" env 2>/dev/null | sed 's/^/      /' || {
    echo "      export OPENAI_BASE_URL=${PROXY_BASE}/v1"
    echo "      export OPENAI_API_KEY=not-needed"
    echo "      export ANTHROPIC_BASE_URL=${PROXY_BASE}"
  }
fi
echo ""
echo -e "  ${DIM}(or: eval \"\$(apexrouter env)\")${NC}"
echo ""
echo -e "  ${BOLD}First five minutes:${NC}"
echo "      apexrouter doctor            what is here, what is missing, one fix line per row"
echo "      apexrouter rig               GPUs, llama.cpp builds, RAM, swap"
echo "      apexrouter models ls         local GGUFs, shards grouped into one row"
echo "      apexrouter up <model> --alias auto"
echo "      apexrouter                   bare = status"
echo ""
echo -e "  ${BOLD}Surfaces:${NC}"
echo "      web UI    ${CONTROL_BASE}/            (no install, served by the daemon)"
$WITH_GUI && echo "      native    $BIN_DIR/$GUI_BIN_NAME            (GPL-3.0-only)"
echo "      agents    claude mcp add apexrouter -- $APEX mcp"
echo "      skill     cp -r $SRC_DIR/skills/apexrouter ~/.claude/skills/"
echo ""
if $WITH_SERVICE; then
  echo -e "  ${DIM}Service:  systemctl --user {status,restart,stop} $SERVICE_NAME${NC}"
  echo -e "  ${DIM}Logs:     journalctl --user -u $SERVICE_NAME -f${NC}"
  $LINGER_ON || echo -e "  ${YELLOW}  ! linger is off — the daemon stops when you log out. Fix: loginctl enable-linger $ME${NC}"
else
  echo -e "  ${DIM}Start:    apexrouter serve --detach${NC}"
fi
[[ -n "$LOG" ]] && echo -e "  ${DIM}Install:  $STATE_DIR/install.log${NC}"
echo -e "  ${DIM}Update:   re-run this installer — it restores your choices and upgrades.${NC}"
echo -e "  ${DIM}Remove:   install.sh --uninstall   (keeps your state; --purge deletes it)${NC}"
echo ""
if $PATH_NEEDS_FIX && ! $MODIFY_PATH; then
  warn "$BIN_DIR is still not on your \$PATH: $PATH_LINE"
fi
if (( ${#LLAMA_FOUND[@]} == 0 )); then
  echo -e "  ${YELLOW}  ! No llama.cpp build yet, so nothing local can serve a model.${NC}"
  if $SERVICE_ACTIVE; then
    echo -e "  ${DIM}    The daemon is up and honest about it: 'apexrouter doctor' says so,${NC}"
    echo -e "  ${DIM}    and /v1/models is simply empty until a backend is bound.${NC}"
  else
    echo -e "  ${DIM}    'apexrouter doctor' says so once the daemon is running.${NC}"
  fi
fi
echo ""
echo "-- ${PRODUCT} install finished $(date -Is) --"

# Give the tee pipe a moment to flush before the process exits.
sleep 0.2 2>/dev/null || true

# ── Exit status ───────────────────────────────────────────────────────────────
# A failed verification is a failed install, and it must be legible to something
# that cannot read: `curl … | bash && echo ok`, a CI step, an agent-driven
# provision. Exiting 0 after printing "0 passed, 1 failed" tells every one of
# them the opposite of what the screen says.
if (( VERIFY_FAIL > 0 )); then
  step "verifying the install" \
       "${VERIFY_FAIL} of $(( VERIFY_PASS + VERIFY_FAIL )) checks failed — each is named above, with its reason" \
       "fix what the failing rows name, then re-run this script; it upgrades in place and is safe to repeat"
  exit 1
fi
exit 0
