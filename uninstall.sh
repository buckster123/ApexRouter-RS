#!/bin/sh
# ApexRouter-RS uninstaller.
#
# Removes the binaries, the shell completions and the systemd --user unit.
# Your config, state, ledger and usage history are KEPT unless you explicitly
# ask for them to go.
#
# Nothing is deleted before the full plan has been printed.
#
#   ./uninstall.sh                 binaries + completions + service. Keeps data.
#   ./uninstall.sh --dry-run       print the plan, touch nothing
#   ./uninstall.sh --purge         ...and delete config, state and cache too
#   ./uninstall.sh --help          every flag
#
# It never touches ~/.vastai-gguf, ~/models or ~/llama.cpp. Those belong to
# other tools and to you.
#
# install.sh can also change one thing that is not a file: `loginctl
# enable-linger`, so a user service survives logout. This script reports it and
# offers to put it back (--disable-linger / --keep-linger); it never changes it
# without being told to.

set -eu

VERSION="0.1.0-mk1"

# ---------------------------------------------------------------- defaults ---

PREFIX="${HOME}/.local"
STATE_CLI=""
ASSUME_YES=0
DRY_RUN=0
KEEP_SERVICE=0
PURGE_STATE=0
PURGE_CONFIG=0
PURGE_CACHE=0
LINGER_CHOICE=""      # "" = ask (or keep, unattended); "disable"; "keep"

# ------------------------------------------------------------------- usage ---

usage() {
    cat <<EOF
ApexRouter-RS uninstaller ${VERSION}

Usage: uninstall.sh [OPTIONS]

By default this removes the installed binaries, the shell completions and the
systemd --user unit, and KEEPS everything that holds your data: config, state,
routing table, ledger, usage history and logs. It tells you where they are.

Options:
  --prefix DIR      Where the binaries were installed. Default: \$HOME/.local
                    (binaries in DIR/bin, completions under DIR/share)
  --state-dir DIR   Where the state was installed (install.sh --state-dir /
                    \$APEXROUTER_HOME). Needed only when that install has no
                    systemd unit to read it back from, and the variable is not
                    in your environment. It also moves the config: see
                    --purge-config
  --purge           Shorthand for --purge-config --purge-state --purge-cache
  --purge-config    Also delete the config, wherever it actually resolved: the
                    \$XDG_CONFIG_HOME/apexrouter DIRECTORY on a stock install,
                    but just <state>/config.toml when \$APEXROUTER_HOME is set,
                    because that is what apexrouter itself reads then
  --purge-state     Also delete the state directory: routing table, endpoint
                    records, LEDGER, usage history, logs, credentials
  --purge-cache     Also delete the cache directory (safe; it is all rebuildable)
  --keep-service    Leave the systemd --user unit alone
  --disable-linger  Also run 'loginctl disable-linger', undoing the per-user
                    setting install.sh turns on so a user service survives
                    logout. Nothing else on your machine needs it unless you
                    turned it on for something else.
  --keep-linger     Never call loginctl, and do not ask about it
  --dry-run         Print the plan and exit without deleting anything
  -y, --yes         Do not ask for confirmation (linger is then KEPT unless
                    --disable-linger was given: a per-user system setting is
                    not changed by default)
  -h, --help        This

Never touched, at any setting:
  ~/.vastai-gguf     another tool's state directory
  ~/models           your weights
  ~/llama.cpp        your llama.cpp checkouts and builds
  ~/.cache/huggingface, ~/.config/vastai   other tools' credential paths

Environment respected: \$APEXROUTER_HOME, \$APEXROUTER_CONFIG,
\$XDG_STATE_HOME, \$XDG_CONFIG_HOME, \$XDG_CACHE_HOME, \$XDG_DATA_HOME —
resolved the way apexrouter-core's paths.rs resolves them. When
\$APEXROUTER_HOME is not in your environment, an 'Environment=APEXROUTER_HOME='
line in the systemd unit is read instead, so an install with its own
--state-dir is found rather than the default one.

Run it with --dry-run first: it prints the resolved paths as the plan.
EOF
}

# ------------------------------------------------------------- arg parsing ---

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            if [ $# -lt 2 ]; then
                echo "uninstall.sh: --prefix needs a directory" >&2
                exit 2
            fi
            PREFIX="$2"; shift 2 ;;
        --prefix=*)     PREFIX="${1#--prefix=}"; shift ;;
        --state-dir)
            if [ $# -lt 2 ]; then
                echo "uninstall.sh: --state-dir needs a directory" >&2
                exit 2
            fi
            STATE_CLI="$2"; shift 2 ;;
        --state-dir=*)  STATE_CLI="${1#--state-dir=}"; shift ;;
        --purge)        PURGE_STATE=1; PURGE_CONFIG=1; PURGE_CACHE=1; shift ;;
        --purge-state)  PURGE_STATE=1;  shift ;;
        --purge-config) PURGE_CONFIG=1; shift ;;
        --purge-cache)  PURGE_CACHE=1;  shift ;;
        --keep-service) KEEP_SERVICE=1; shift ;;
        --disable-linger) LINGER_CHOICE="disable"; shift ;;
        --keep-linger)    LINGER_CHOICE="keep";    shift ;;
        --dry-run)      DRY_RUN=1;      shift ;;
        -y|--yes)       ASSUME_YES=1;   shift ;;
        -h|--help)      usage; exit 0 ;;
        *)
            echo "uninstall.sh: unknown option '$1'" >&2
            echo "try: uninstall.sh --help" >&2
            exit 2 ;;
    esac
done

PREFIX="${PREFIX%/}"

# --------------------------------------------------------- path resolution ---
#
# Mirrors apexrouter-core's paths.rs, so that what this removes is what that
# wrote:
#
#   config  $APEXROUTER_CONFIG -> $APEXROUTER_HOME/config.toml
#           -> $XDG_CONFIG_HOME/apexrouter -> ~/.config/apexrouter
#   state   $APEXROUTER_HOME -> $XDG_STATE_HOME/apexrouter
#           -> ~/.local/state/apexrouter
#   cache   $XDG_CACHE_HOME/apexrouter -> ~/.cache/apexrouter
#
# XDG says a relative value is invalid, hence the leading-slash test.

xdg_dir() {
    # $1 = env value (possibly empty), $2 = fallback
    case "${1:-}" in
        /*) printf '%s\n' "$1" ;;
        *)  printf '%s\n' "$2" ;;
    esac
}

CONFIG_HOME="$(xdg_dir "${XDG_CONFIG_HOME:-}" "${HOME}/.config")"
DATA_HOME="$(xdg_dir "${XDG_DATA_HOME:-}" "${HOME}/.local/share")"

BIN_DIR="${PREFIX}/bin"
UNIT_DIR="${CONFIG_HOME}/systemd/user"
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"

# install.sh names the unit `apexrouter.service`. `apexrouterd.service` is
# accepted too, because that is what an early draft of docs/INSTALL.md told
# people to write by hand.
SERVICE_NAMES="apexrouter apexrouterd"

# An install with a non-default --state-dir has `Environment=APEXROUTER_HOME=…`
# in its unit file, and install.sh writes that line ONLY then. So when the var
# is not in our environment the unit is the authoritative record of where that
# install put its state — and without reading it we would resolve the DEFAULT
# state dir and offer to delete a different install's data.
STATE_FROM_UNIT=""
if [ -z "$STATE_CLI" ] && [ -z "${APEXROUTER_HOME:-}" ]; then
    for svc in $SERVICE_NAMES; do
        unit="${UNIT_DIR}/${svc}.service"
        [ -f "$unit" ] || continue
        v=$(sed -n 's/^[[:space:]]*Environment=APEXROUTER_HOME=//p' "$unit" | tail -n 1)
        # Strip one layer of surrounding quotes; systemd allows either.
        v="${v%\"}"; v="${v#\"}"; v="${v%\'}"; v="${v#\'}"
        # systemd's %h is the user's home; a hand-written unit may well use it.
        case "$v" in
            '%h'/*) v="${HOME}${v#%h}" ;;
        esac
        case "$v" in
            /*) STATE_FROM_UNIT="${v%/}"; break ;;
        esac
    done
fi

if [ -n "$STATE_CLI" ]; then
    STATE_DIR="${STATE_CLI%/}"
    STATE_SRC="--state-dir"
elif [ -n "${APEXROUTER_HOME:-}" ]; then
    STATE_DIR="${APEXROUTER_HOME%/}"
    STATE_SRC="\$APEXROUTER_HOME"
elif [ -n "$STATE_FROM_UNIT" ]; then
    STATE_DIR="$STATE_FROM_UNIT"
    STATE_SRC="APEXROUTER_HOME in the systemd unit"
else
    STATE_DIR="$(xdg_dir "${XDG_STATE_HOME:-}" "${HOME}/.local/state")/apexrouter"
    STATE_SRC="the default"
fi

CACHE_DIR="$(xdg_dir "${XDG_CACHE_HOME:-}" "${HOME}/.cache")/apexrouter"

# Config is the one that bites. paths.rs makes the config file
# $APEXROUTER_HOME/config.toml the moment APEXROUTER_HOME is set AT ALL, so an
# install with its own state dir has NO ~/.config/apexrouter — that directory,
# if it exists, belongs to some OTHER install and is not ours to delete.
#
#   $APEXROUTER_CONFIG      -> that exact file, and no directory is ours
#   $APEXROUTER_HOME set    -> $STATE_DIR/config.toml, a file inside the state dir
#   otherwise               -> $XDG_CONFIG_HOME/apexrouter, a directory we own
#
# CONFIG_DIR is set only in the third case; CONFIG_TARGET is what --purge-config
# actually removes, and is reported as "still on disk" afterwards.
CONFIG_DIR=""
if [ -n "${APEXROUTER_CONFIG:-}" ]; then
    CONFIG_TARGET="$APEXROUTER_CONFIG"
    CONFIG_SRC="\$APEXROUTER_CONFIG"
elif [ -n "$STATE_CLI" ] || [ -n "${APEXROUTER_HOME:-}" ] || [ -n "$STATE_FROM_UNIT" ]; then
    CONFIG_TARGET="${STATE_DIR}/config.toml"
    CONFIG_SRC="inside the state dir — APEXROUTER_HOME moves the config too"
else
    CONFIG_DIR="${CONFIG_HOME}/apexrouter"
    CONFIG_TARGET="$CONFIG_DIR"
    CONFIG_SRC="the default"
fi

# ------------------------------------------------------------ safety rails ---
#
# Paths this script refuses to remove no matter what is asked of it.
# `~/.vastai-gguf` is on the list because it is ANOTHER TOOL'S state directory:
# ApexRouter reads it and never writes it, and an uninstaller that "tidied up"
# would destroy the very thing a migration reads from.

is_forbidden() {
    p="$1"

    # Explicitly ours, even under an unusual --prefix.
    case "$p" in
        "${BIN_DIR}"/apexrouter|"${BIN_DIR}"/apexrouter-ui) return 1 ;;
        "${PREFIX}"/share/*apexrouter*)                     return 1 ;;
        "${CARGO_BIN}"/apexrouter|"${CARGO_BIN}"/apexrouter-ui) return 1 ;;
    esac

    case "$p" in
        ""|"/"|"$HOME"|"$HOME"/)                     return 0 ;;
        "$HOME"/.vastai-gguf|"$HOME"/.vastai-gguf/*) return 0 ;;
        "$HOME"/models|"$HOME"/models/*)             return 0 ;;
        "$HOME"/llama.cpp|"$HOME"/llama.cpp/*)       return 0 ;;
        "$HOME"/.cache/huggingface*)                 return 0 ;;
        "$HOME"/.config/vastai*)                     return 0 ;;
        /bin/*|/sbin/*|/usr/*|/etc/*|/var/*|/lib/*|/opt/*) return 0 ;;
        *) return 1 ;;
    esac
}

# -------------------------------------------------------------------- plan ---
#
# Three newline-separated accumulators. `|` separates path from reason in
# PLAN_KEEP; no path we generate contains one.

NL='
'
PLAN_REMOVE=""
PLAN_KEEP=""
PLAN_ACTIONS=""

plan_remove() {
    if [ ! -e "$1" ] && [ ! -L "$1" ]; then
        return 0
    fi
    if is_forbidden "$1"; then
        printf 'uninstall.sh: refusing to remove protected path: %s\n' "$1" >&2
        return 0
    fi
    # De-duplicate: with the default --prefix, $PREFIX/share and $XDG_DATA_HOME
    # are the same directory and several probes land on one path.
    case "${NL}${PLAN_REMOVE}" in
        *"${NL}${1}${NL}"*) return 0 ;;
    esac
    PLAN_REMOVE="${PLAN_REMOVE}${1}${NL}"
}

plan_keep() {
    if [ ! -e "$1" ]; then
        return 0
    fi
    case "${NL}${PLAN_KEEP}" in
        *"${NL}${1}|"*) return 0 ;;
    esac
    PLAN_KEEP="${PLAN_KEEP}${1}|${2}${NL}"
}

plan_action() {
    PLAN_ACTIONS="${PLAN_ACTIONS}${1}${NL}"
}

# ---- binaries ---------------------------------------------------------------

plan_remove "${BIN_DIR}/apexrouter"
plan_remove "${BIN_DIR}/apexrouter-ui"

# `cargo install --path crates/apexrouter-cli` is the other documented way in,
# and it lands somewhere --prefix does not cover.
if [ "$CARGO_BIN" != "$BIN_DIR" ]; then
    plan_remove "${CARGO_BIN}/apexrouter"
    plan_remove "${CARGO_BIN}/apexrouter-ui"
fi

# ---- shell completions ------------------------------------------------------

plan_remove "${PREFIX}/share/bash-completion/completions/apexrouter"
plan_remove "${PREFIX}/share/zsh/site-functions/_apexrouter"
plan_remove "${DATA_HOME}/bash-completion/completions/apexrouter"
plan_remove "${DATA_HOME}/zsh/site-functions/_apexrouter"
plan_remove "${CONFIG_HOME}/fish/completions/apexrouter.fish"

# ---- desktop entry, if an installer wrote one -------------------------------

plan_remove "${DATA_HOME}/applications/apexrouter-ui.desktop"

# ---- systemd --user unit ----------------------------------------------------

SERVICE_PRESENT=0
SERVICE_UNITS=""
for svc in $SERVICE_NAMES; do
    unit="${UNIT_DIR}/${svc}.service"
    if [ ! -f "$unit" ]; then
        continue
    fi
    if [ "$KEEP_SERVICE" -eq 1 ]; then
        plan_keep "$unit" "--keep-service"
        continue
    fi
    SERVICE_PRESENT=1
    SERVICE_UNITS="${SERVICE_UNITS}${svc} "
    plan_action "systemctl --user disable --now ${svc}"
    plan_remove "$unit"
    # install.sh keeps a timestamped backup when it rewrites a unit.
    for bak in "${unit}".bak.*; do
        if [ -f "$bak" ]; then
            plan_remove "$bak"
        fi
    done
done

# ---- data -------------------------------------------------------------------

if [ "$PURGE_STATE" -eq 1 ]; then
    plan_remove "$STATE_DIR"
else
    plan_keep "$STATE_DIR" "routing table, endpoint records, ledger, usage, logs, credentials, install.conf"
fi

# When the config lives inside the state dir, --purge-state already takes it and
# naming it twice in the plan is noise.
CONFIG_UNDER_STATE=0
case "$CONFIG_TARGET" in
    "${STATE_DIR}"/*) CONFIG_UNDER_STATE=1 ;;
esac

if [ "$PURGE_CONFIG" -eq 1 ]; then
    if [ "$CONFIG_UNDER_STATE" -eq 0 ] || [ "$PURGE_STATE" -eq 0 ]; then
        plan_remove "$CONFIG_TARGET"
    fi
elif [ "$CONFIG_UNDER_STATE" -eq 0 ] || [ "$PURGE_STATE" -eq 0 ]; then
    plan_keep "$CONFIG_TARGET" "your config.toml (${CONFIG_SRC})"
fi

if [ "$PURGE_CACHE" -eq 1 ]; then
    plan_remove "$CACHE_DIR"
else
    plan_keep "$CACHE_DIR" "HF metadata, probe results, cached offers - all rebuildable"
fi

# ------------------------------------------------------- what is still live --
#
# Three things outlive this script and cost you memory or money. Say so BEFORE
# deleting the binary that can see them, because afterwards you cannot.

APEXROUTER_BIN=""
for cand in "${BIN_DIR}/apexrouter" "${CARGO_BIN}/apexrouter"; do
    if [ -x "$cand" ]; then
        APEXROUTER_BIN="$cand"
        break
    fi
done
if [ -z "$APEXROUTER_BIN" ]; then
    APEXROUTER_BIN="$(command -v apexrouter 2>/dev/null || true)"
fi

WARNINGS=""
warn() { WARNINGS="${WARNINGS}  ! ${1}${NL}"; }

# 1. the daemon itself.
DAEMON_LIVE=0
if [ -f "${STATE_DIR}/apexrouterd.lock" ]; then
    DAEMON_LIVE=1
fi

# 2. llama-server children. Spawned with setsid() and DELIBERATELY outliving
#    the manager ([supervisor] kill_children_on_exit defaults to false), so
#    removing ApexRouter does not free their VRAM.
ORPHANS=0
if [ -d "${STATE_DIR}/endpoints" ]; then
    ORPHANS=$(find "${STATE_DIR}/endpoints" -name '*.json' 2>/dev/null | wc -l | tr -d ' ')
fi
if [ "${ORPHANS:-0}" -gt 0 ]; then
    warn "${ORPHANS} endpoint record(s) exist. llama-server children are spawned with"
    warn "  setsid() and outlive the manager on purpose, so they keep holding VRAM"
    warn "  after this. Stop them first:   apexrouter endpoint ls"
    warn "                                 apexrouter endpoint stop <id>"
fi

# 3. rented vast.ai instances. Best-effort read of the append-only ledger: the
#    last row per instance id wins. Nothing that costs money is ever
#    auto-destroyed - a feature, right up until you forget one exists.
LEDGER="${STATE_DIR}/ledger.jsonl"
if [ -s "$LEDGER" ] && command -v awk >/dev/null 2>&1; then
    LIVE_ROWS=$(awk '
        {
            id = ""; st = ""
            if (match($0, /"instance_id":[ ]*[0-9]+/)) {
                s = substr($0, RSTART, RLENGTH); sub(/.*:[ ]*/, "", s); id = s
            }
            if (match($0, /"state":[ ]*"[a-z_]+"/)) {
                s = substr($0, RSTART, RLENGTH)
                sub(/.*"state":[ ]*"/, "", s); sub(/"$/, "", s); st = s
            }
            if (id != "" && st != "") { last[id] = st }
        }
        END { for (i in last) if (last[i] != "destroyed") printf "%s(%s) ", i, last[i] }
    ' "$LEDGER" 2>/dev/null || true)
    if [ -n "${LIVE_ROWS:-}" ]; then
        warn "the ledger has rows never marked destroyed: ${LIVE_ROWS}"
        warn "  Uninstalling ApexRouter does NOT destroy a rented box, and it keeps"
        warn "  billing. Check before you continue:   apexrouter vast ls"
        warn "  (that reads the ledger and answers with the daemon down)"
    fi
fi

if [ "$PURGE_STATE" -eq 1 ] && [ -s "$LEDGER" ]; then
    warn "--purge-state deletes ${LEDGER}, the only local record of what you have"
    warn "  rented. Copy it somewhere first if there is any chance a box is alive."
fi

# 4. install.sh may have appended one marked line to a shell rc file. Editing
#    somebody's rc file from an uninstaller is a good way to eat a shell
#    function, so we find it and point at it rather than rewriting it.
RC_MARKER="# added by ApexRouter-RS install.sh"
for rc in "${HOME}/.bashrc" "${HOME}/.zshrc" "${ZDOTDIR:-$HOME}/.zshrc" "${HOME}/.profile"; do
    if [ -f "$rc" ] && grep -Fq "$RC_MARKER" "$rc" 2>/dev/null; then
        warn "${rc} has a PATH line marked '${RC_MARKER}'."
        warn "  Remove it by hand if you want it gone - this script does not edit"
        warn "  shell rc files."
    fi
done

# 5. linger. install.sh runs `loginctl enable-linger` for you when it installs
#    the service unattended (a user service otherwise stops when your last
#    session ends). It is a per-user SYSTEM setting, it outlives this script,
#    and nothing else here would ever tell you it had been changed. So: say so,
#    and offer to put it back. Never silently - that would be the same fault in
#    the other direction.
LINGER_ON=0
LINGER_ASK=0
if command -v loginctl >/dev/null 2>&1; then
    if [ "$(loginctl show-user "${USER:-$(id -un)}" -p Linger --value 2>/dev/null || echo no)" = "yes" ]; then
        LINGER_ON=1
    fi
fi
ME="${USER:-$(id -un)}"

if [ "$LINGER_ON" -eq 1 ]; then
    case "$LINGER_CHOICE" in
        disable)
            plan_action "loginctl disable-linger ${ME}" ;;
        keep)
            warn "linger stays enabled for ${ME} (--keep-linger)." ;;
        *)
            if [ "$ASSUME_YES" -eq 0 ] && [ "$DRY_RUN" -eq 0 ] && [ -t 0 ]; then
                LINGER_ASK=1
                warn "linger is enabled for ${ME}. install.sh turns it on so a user service"
                warn "  survives logout; it is a per-user system setting and it outlives this"
                warn "  uninstall. You will be asked about it after the confirmation below."
            else
                warn "linger is enabled for ${ME}. install.sh turns it on so a user service"
                warn "  survives logout, and this script does not change it on its own. Undo:"
                warn "                                 loginctl disable-linger ${ME}"
                warn "  (or re-run with --disable-linger)"
            fi ;;
    esac
fi

# ------------------------------------------------------------ print the plan --

echo "ApexRouter-RS uninstaller ${VERSION}"
echo

# Where it decided to look, and on whose authority. An uninstaller pointed at
# the wrong install is the failure that matters here, and this is the line that
# lets you catch it before the confirmation rather than after.
echo "Resolved:"
echo "  binaries  ${BIN_DIR}"
echo "  state     ${STATE_DIR}    (${STATE_SRC})"
echo "  config    ${CONFIG_TARGET}    (${CONFIG_SRC})"
echo "  cache     ${CACHE_DIR}"
echo

STOP_BY_CLI=0
if [ "$DAEMON_LIVE" -eq 1 ] && [ "$SERVICE_PRESENT" -eq 0 ] && [ -n "$APEXROUTER_BIN" ]; then
    STOP_BY_CLI=1
fi
if [ "$DAEMON_LIVE" -eq 1 ] && [ "$SERVICE_PRESENT" -eq 0 ] && [ -z "$APEXROUTER_BIN" ]; then
    warn "${STATE_DIR}/apexrouterd.lock exists but no apexrouter binary was found,"
    warn "  so this script cannot stop a daemon that may still be running. If one is,"
    warn "  stop it yourself before removing its state."
fi

if [ -n "$PLAN_ACTIONS" ] || [ "$STOP_BY_CLI" -eq 1 ]; then
    echo "Will run:"
    if [ -n "$PLAN_ACTIONS" ]; then
        printf '%s' "$PLAN_ACTIONS" | while IFS= read -r a; do
            if [ -n "$a" ]; then echo "  ${a}"; fi
        done
    fi
    if [ "$STOP_BY_CLI" -eq 1 ]; then
        echo "  ${APEXROUTER_BIN} serve --stop"
    fi
    echo
fi

if [ -n "$PLAN_REMOVE" ]; then
    echo "Will DELETE:"
    printf '%s' "$PLAN_REMOVE" | while IFS= read -r p; do
        if [ -z "$p" ]; then continue; fi
        if [ -d "$p" ]; then
            sz="$(du -sh "$p" 2>/dev/null | cut -f1)"
            echo "  ${p}/    (directory, ${sz:-?})"
        else
            echo "  ${p}"
        fi
    done
    echo
else
    echo "Nothing to delete - no ApexRouter-RS install found under ${PREFIX}."
    echo
fi

if [ -n "$PLAN_KEEP" ]; then
    echo "Will KEEP - this is your data. Pass --purge to remove it too:"
    printf '%s' "$PLAN_KEEP" | while IFS= read -r line; do
        if [ -z "$line" ]; then continue; fi
        echo "  ${line%%|*}    - ${line#*|}"
    done
    echo
fi

echo "Never touched: ~/.vastai-gguf  ~/models  ~/llama.cpp"
echo

if [ -n "$WARNINGS" ]; then
    echo "Before you continue:"
    printf '%s' "$WARNINGS"
    echo
fi

if [ "$DRY_RUN" -eq 1 ]; then
    echo "--dry-run: nothing was changed."
    exit 0
fi

if [ -z "$PLAN_REMOVE" ] && [ -z "$PLAN_ACTIONS" ] && [ "$STOP_BY_CLI" -eq 0 ]; then
    exit 0
fi

# ------------------------------------------------------------- confirmation --

if [ "$ASSUME_YES" -eq 0 ]; then
    if [ ! -t 0 ]; then
        echo "uninstall.sh: stdin is not a terminal and --yes was not given." >&2
        echo "Refusing to delete anything without confirmation." >&2
        exit 1
    fi
    printf 'Proceed? [y/N] '
    read -r reply
    case "$reply" in
        y|Y|yes|YES) ;;
        *) echo "Aborted. Nothing was changed."; exit 1 ;;
    esac
    echo
fi

# ------------------------------------------------------------------ execute --

# 1. Stop the daemon. Its children are left alone on purpose - see above.
if [ "$SERVICE_PRESENT" -eq 1 ] && command -v systemctl >/dev/null 2>&1; then
    echo "stopping the service..."
    for svc in $SERVICE_UNITS; do
        systemctl --user disable --now "$svc" >/dev/null 2>&1 || true
    done
elif [ "$STOP_BY_CLI" -eq 1 ]; then
    echo "stopping the daemon..."
    "$APEXROUTER_BIN" serve --stop >/dev/null 2>&1 || true
fi

# 2. Remove the planned paths. Word-split on newline only, so a path with a
#    space in it survives; globbing off, so one with a '*' does too.
FAILED=0
OLD_IFS="$IFS"
IFS="$NL"
set -f
for p in $PLAN_REMOVE; do
    if [ -z "$p" ]; then continue; fi
    if is_forbidden "$p"; then
        echo "  refusing: ${p}" >&2
        continue
    fi
    if rm -rf -- "$p" 2>/dev/null; then
        echo "  removed ${p}"
    else
        echo "  FAILED to remove ${p}" >&2
        FAILED=1
    fi
done
set +f
IFS="$OLD_IFS"

# 3. Tell systemd the unit is gone.
if [ "$SERVICE_PRESENT" -eq 1 ] && command -v systemctl >/dev/null 2>&1; then
    systemctl --user daemon-reload >/dev/null 2>&1 || true
fi

# 4. linger. Asked here rather than up front so that the plan printed above
#    stays exactly true: this changes a per-user system setting, not a file.
if [ "$LINGER_ASK" -eq 1 ]; then
    echo
    echo "linger is enabled for ${ME} - your user services keep running after you"
    echo "log out. install.sh turns it on when it installs the service. If you did"
    echo "not turn it on for anything else, it can go."
    printf 'Disable linger for %s? [y/N] ' "$ME"
    read -r reply
    echo
    case "$reply" in
        y|Y|yes|YES) LINGER_CHOICE="disable" ;;
        *) echo "  left enabled - undo later with: loginctl disable-linger ${ME}" ;;
    esac
fi

if [ "$LINGER_ON" -eq 1 ] && [ "$LINGER_CHOICE" = "disable" ]; then
    if loginctl disable-linger "$ME" >/dev/null 2>&1; then
        echo "  disabled linger for ${ME}"
    else
        echo "  could not disable linger (needs polkit or root):" >&2
        echo "    sudo loginctl disable-linger ${ME}" >&2
    fi
fi

echo

# Report what is ACTUALLY still on disk rather than what was asked for. A
# --purge that hit a safety rail must not claim the directory is gone.
STILL_HERE=""
if [ -e "$CONFIG_TARGET" ]; then STILL_HERE="${STILL_HERE}  config  ${CONFIG_TARGET}${NL}"; fi
if [ -d "$STATE_DIR" ];    then STILL_HERE="${STILL_HERE}  state   ${STATE_DIR}${NL}";      fi
if [ -d "$CACHE_DIR" ];    then STILL_HERE="${STILL_HERE}  cache   ${CACHE_DIR}${NL}";      fi

if [ -n "$STILL_HERE" ]; then
    echo "Done. Still on disk:"
    printf '%s' "$STILL_HERE"
    echo
    echo "Reinstalling later picks all of it back up. To remove it now:"
    echo "  ./uninstall.sh --purge"
else
    echo "Done. Config, state and cache were removed."
fi

if [ "$FAILED" -eq 1 ]; then
    echo
    echo "Some paths could not be removed - see the messages above." >&2
    exit 1
fi

exit 0
