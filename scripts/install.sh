#!/bin/sh
# Installs or updates Tworky on macOS or Linux from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/tristanmenzel/Tworky/main/scripts/install.sh | sh
#
# The app runs it with --wait-pid and --status-dir: it downloads while the app keeps running,
# reports progress in <status-dir>/status.json, then installs once the app has exited and reopens
# it if the app left a `relaunch` file.
#
# Downloading with curl sets no com.apple.quarantine, so Gatekeeper never sees the unsigned app.
set -u

repo=tristanmenzel/Tworky
wait_pid=
status_dir=
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) repo=$2; shift 2 ;;
    --wait-pid) wait_pid=$2; shift 2 ;;
    --status-dir) status_dir=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
from_app=
[ -n "$wait_pid" ] && [ -n "$status_dir" ] && from_app=1

log() {
  echo "$1"
  if [ -n "$status_dir" ]; then echo "$1" >> "$status_dir/update.log"; fi
}

json_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | tr '\n' ' '
}

set_phase() {
  [ -n "$status_dir" ] || return 0
  printf '{"phase":"%s","message":"%s"}' "$1" "$(json_escape "${2:-}")" > "$status_dir/status.json.tmp"
  mv -f "$status_dir/status.json.tmp" "$status_dir/status.json"
}

os=$(uname -s)
case "$os" in
  Darwin) pattern='Tworky-universal.app.tar.gz' ;;
  Linux) pattern='*_amd64.AppImage' ;;
  *) echo "Tworky has no build for $os" >&2; exit 1 ;;
esac

app_dir=/Applications
if [ "$os" = Darwin ] && [ ! -w "$app_dir" ]; then app_dir="$HOME/Applications"; fi
appimage="$HOME/.local/share/Tworky/Tworky.AppImage"

app_running() {
  if [ -n "$wait_pid" ]; then
    kill -0 "$wait_pid" 2>/dev/null
  elif [ "$os" = Darwin ]; then
    pgrep -x tworky >/dev/null 2>&1
  else
    pgrep -f "$appimage" >/dev/null 2>&1
  fi
}

start_app() {
  if [ "$os" = Darwin ]; then
    if [ -d "$app_dir/Tworky.app" ]; then log "Starting $app_dir/Tworky.app"; open "$app_dir/Tworky.app"; fi
  elif [ -x "$appimage" ]; then
    log "Starting $appimage"
    nohup "$appimage" >/dev/null 2>&1 &
  fi
}

fail() {
  log "Update failed: $1"
  set_phase failed "$1"
  finish
  exit 1
}

finish() {
  relaunch=
  if [ -z "$from_app" ]; then
    [ -n "$installed" ] && relaunch=1
  elif [ -f "$status_dir/relaunch" ]; then
    rm -f "$status_dir/relaunch"
    app_running || relaunch=1
  fi
  [ -n "$relaunch" ] && start_app
  [ -n "${download:-}" ] && rm -rf "$download"
}

installed=
download=
set_phase downloading
download=$(mktemp -d "${TMPDIR:-/tmp}/tworky.XXXXXX") || fail 'could not make a download folder'
releases=$(curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=20" </dev/null) \
  || fail "could not list the releases of $repo"
# The API doesn't list releases in publish order, so take the newest by published_at that has this
# platform's build. The Windows build is published first, so the newest release may lack it for a
# while. A release's published_at precedes its assets in the JSON; drafts are hidden.
url=
published=
newest=
for token in $(printf '%s' "$releases" \
  | grep -oE '"(published_at|browser_download_url)": *"[^"]*"' \
  | sed 's/^"\([a-z_]*\)": *"\([^"]*\)"$/\1=\2/'); do
  case "$token" in
    published_at=*) published=${token#published_at=} ;;
    browser_download_url=*)
      candidate=${token#browser_download_url=}
      # shellcheck disable=SC2254 # $pattern is a glob on purpose
      case "${candidate##*/}" in
        $pattern)
          if [ -z "$url" ] || [ "$published" \> "$newest" ]; then url=$candidate; newest=$published; fi ;;
      esac ;;
  esac
done
[ -n "$url" ] || fail "No recent release has a $pattern build"
log "Downloading $url"
if [ -n "$status_dir" ]; then
  curl -fsSL -o "$download/${url##*/}" "$url" </dev/null >> "$status_dir/update.log" 2>&1
else
  curl -fL -o "$download/${url##*/}" "$url" </dev/null
fi || fail 'downloading the release failed'
asset=$(find "$download" -type f | head -n 1)
[ -n "$asset" ] || fail "the download held no $pattern"

set_phase ready
if app_running; then
  if [ -n "$wait_pid" ]; then
    log 'Waiting for Tworky to close'
  else
    log 'Tworky is running. Quit it to continue; the update installs once it has exited.'
  fi
  while app_running; do sleep 1; done
fi

set_phase installing
if [ "$os" = Darwin ]; then
  log "Installing into $app_dir"
  mkdir -p "$app_dir" "$download/app" || fail "could not create $app_dir"
  tar -xzf "$asset" -C "$download/app" || fail 'could not extract the app'
  [ -d "$download/app/Tworky.app" ] || fail 'the download held no Tworky.app'
  rm -rf "$app_dir/Tworky.app" || fail "could not remove the old $app_dir/Tworky.app"
  mv "$download/app/Tworky.app" "$app_dir/Tworky.app" || fail "could not move Tworky.app into $app_dir"
  xattr -dr com.apple.quarantine "$app_dir/Tworky.app" 2>/dev/null || true
else
  log "Installing $appimage"
  mkdir -p "$(dirname "$appimage")" "$HOME/.local/bin" "$HOME/.local/share/applications"
  cp "$asset" "$appimage.new" && chmod +x "$appimage.new" && mv -f "$appimage.new" "$appimage" \
    || fail "could not write $appimage"
  ln -sf "$appimage" "$HOME/.local/bin/tworky"
  desktop="$HOME/.local/share/applications/tworky.desktop"
  if [ ! -f "$desktop" ]; then
    printf '[Desktop Entry]\nType=Application\nName=Tworky\nExec=%s\nTerminal=false\nCategories=Development;\n' \
      "$appimage" > "$desktop"
  fi
fi
installed=1
set_phase done
log 'Tworky is installed.'
finish
