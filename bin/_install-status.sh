# sourced by install-local.sh: what `--status` prints.  Reads APP,
# PLIST and MACOS, and calls bundle_bin, all set by the caller.

cmd_status() {
  echo "App bundle:    $APP"
  if [[ -d "$APP" ]]; then
    echo "  CFBundleExecutable: $(/usr/libexec/PlistBuddy -c 'Print CFBundleExecutable' "$PLIST" 2>/dev/null || echo '?')"
    for b in marspot-shell marspot-core; do
      local p; p="$(bundle_bin "$b")"
      if [[ -f "$p" && ! -L "$p" ]]; then
        echo "  $b: $(stat -f '%z' "$p") B"
      elif [[ -L "$p" ]]; then
        echo "  $b: SYMLINK → $(readlink "$p")  (should be a real copy)"
      else
        echo "  $b: (absent)"
      fi
    done
  else
    echo "  (not installed)"
  fi
  echo "Running:"
  "$MACOS/marspot-shell" --status 2>/dev/null | sed -n '3,6p' | sed 's/^/  /' || echo "  (shell --status unavailable)"
}
