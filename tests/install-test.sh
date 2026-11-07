#!/bin/bash
# Install test for the Umoja .debs on a stock Ubuntu desktop.
#
# Run as root in a fresh Ubuntu container (no systemd needed):
#   tests/install-test.sh <dir-with-debs> <output-dir>
#
# 1. Installs ubuntu-desktop-minimal with Recommends, like a real install.
# 2. Installs umoja-desktop + umoja-greeter with Recommends and checks that
#    apt resolves every dependency, that the shipped binaries find all of
#    their libraries, and that LightDM became the default display manager.
# 3. Runs a headless Umoja session as a normal user (umoja-mir on Mir's
#    virtual output, software GL) with the panel, dock, overlay and GNOME
#    apps, captures screenshots with grim, plus the greeter in preview mode
#    under Xvfb. See tests/headless-session.sh.
#
# Set SKIP_DESKTOP=1 if ubuntu-desktop-minimal is already installed.
set -uo pipefail

DEBS=$(realpath "${1:?deb directory}")
OUT=$(realpath -m "${2:?output directory}")
mkdir -p "$OUT"
export DEBIAN_FRONTEND=noninteractive
LOG="$OUT/install-test.log"
FAILED=0

step() { echo; echo "=== $*" | tee -a "$LOG"; }
fail() { echo "FAIL: $*" | tee -a "$LOG"; FAILED=1; }
pass() { echo "PASS: $*" | tee -a "$LOG"; }

step "Install ubuntu-desktop-minimal (with Recommends)"
if [ -z "${SKIP_DESKTOP:-}" ]; then
    apt-get update -q
    apt-get install -y -q ubuntu-desktop-minimal >>"$LOG" 2>&1 \
        || { fail "ubuntu-desktop-minimal did not install"; exit 1; }
fi
dpkg -s gdm3 >/dev/null 2>&1 && echo "gdm3 present: $(cat /etc/X11/default-display-manager 2>/dev/null)" | tee -a "$LOG"

step "Install the Umoja packages (with Recommends)"
ls -1 "$DEBS"/*.deb | tee -a "$LOG"
# Simulate first so the log shows exactly what apt pulls in.
apt-get install -s "$DEBS"/umoja-desktop_*.deb "$DEBS"/umoja-greeter_*.deb \
    | grep -E '^(Inst|Remv)' | tee "$OUT/apt-plan.txt" >>"$LOG"
echo "apt would install $(grep -c '^Inst' "$OUT/apt-plan.txt") packages, remove $(grep -c '^Remv' "$OUT/apt-plan.txt" || true)" | tee -a "$LOG"
if apt-get install -y -q "$DEBS"/umoja-desktop_*.deb "$DEBS"/umoja-greeter_*.deb >>"$LOG" 2>&1; then
    pass "umoja-desktop and umoja-greeter installed"
else
    fail "apt-get install of the Umoja debs failed (see log)"
    tail -40 "$LOG"
    exit 1
fi
apt-get check >>"$LOG" 2>&1 && pass "apt-get check is clean" || fail "apt-get check reports broken packages"
dpkg -l | awk '/^.[^i] *umoja/ {print}' | grep . && fail "Umoja packages not fully configured"

step "Recommends that got installed"
for p in umoja-greeter grim wireplumber power-profiles-daemon gnome-keyring; do
    if dpkg -s "$p" >/dev/null 2>&1; then pass "recommend $p installed"; else fail "recommend $p missing"; fi
done
# Myna isn't in the Ubuntu archive; dictation is simply unavailable without it.
dpkg -s myna-config >/dev/null 2>&1 && pass "recommend myna-config installed" \
    || echo "NOTE: myna-config not available from the configured archives" | tee -a "$LOG"

step "Shared libraries"
for b in /usr/bin/umoja-mir /usr/bin/umoja-panel /usr/bin/umoja-dock /usr/bin/umoja-overlay \
         /usr/libexec/xdg-desktop-portal-umoja /usr/sbin/umoja-greeter /usr/lib/umoja/lemonade/lemond; do
    missing=$(ldd "$b" 2>&1 | grep 'not found')
    if [ -n "$missing" ]; then fail "$b: $missing"; else pass "$b links"; fi
done
/usr/lib/umoja/lemonade/lemond --version >>"$LOG" 2>&1 && pass "embedded lemond runs" || fail "embedded lemond does not run"

step "Display manager"
dm=$(cat /etc/X11/default-display-manager 2>/dev/null)
[ "$dm" = /usr/sbin/lightdm ] && pass "default display manager is LightDM" || fail "default display manager is '$dm'"
greeter=$(update-alternatives --query lightdm-greeter 2>/dev/null | awk '/^Value:/ {print $2}')
[[ "$greeter" == *umoja* ]] && pass "lightdm-greeter alternative is $greeter" || fail "lightdm-greeter alternative is '$greeter'"
[ -f /usr/share/wayland-sessions/umoja.desktop ] && pass "Umoja session registered" || fail "umoja.desktop missing"
gsettings --schemadir /usr/share/glib-2.0/schemas list-keys org.umoja.shell >/dev/null 2>&1 \
    && pass "org.umoja.shell schema compiled" || fail "org.umoja.shell schema not compiled"

step "Headless session"
apt-get install -y -q --no-install-recommends mir-platform-graphics-virtual xvfb x11-utils \
    imagemagick dbus-x11 >>"$LOG" 2>&1 || fail "could not install headless test tools"
# Run the session as a normal user, as LightDM would (Nautilus refuses root).
id umoja-test >/dev/null 2>&1 || useradd -m -s /bin/bash -c "Umoja Test" umoja-test
chmod 777 "$OUT"
HERE=$(dirname "$(realpath "$0")")
if runuser -u umoja-test -- "$HERE/headless-session.sh" "$OUT"; then
    pass "headless session checks"
else
    fail "headless session checks (see $OUT/session.log)"
fi

step "Greeter preview"
Xvfb :99 -screen 0 1920x1080x24 -nolisten tcp >>"$LOG" 2>&1 &
XVFB=$!
sleep 2
DISPLAY=:99 GDK_BACKEND=x11 NO_AT_BRIDGE=1 umoja-greeter --preview >"$OUT/umoja-greeter.log" 2>&1 &
GREETER=$!
sleep 8
import -display :99 -window root "$OUT/05-greeter.png" 2>>"$LOG" \
    && pass "captured 05-greeter.png" || fail "could not capture the greeter"
kill $GREETER $XVFB 2>/dev/null

step "Result"
if [ $FAILED = 0 ]; then echo "ALL CHECKS PASSED" | tee -a "$LOG"; else echo "SOME CHECKS FAILED" | tee -a "$LOG"; fi
grep -E '^(PASS|FAIL)' "$LOG" > "$OUT/summary.txt"
exit $FAILED
