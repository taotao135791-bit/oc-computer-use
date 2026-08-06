#!/usr/bin/env bash
# Restore system defaults touched by system-settings tasks. Harmless to run
# when a key was never set (defaults delete fails silently on missing keys).
set +e
defaults delete -g AppleInterfaceStyle 2>/dev/null
defaults delete com.apple.screencapture location 2>/dev/null
defaults delete com.apple.menuextra.clock ShowSeconds 2>/dev/null
defaults delete com.apple.finder AppleShowAllFiles 2>/dev/null
killall Finder 2>/dev/null
killall System Settings 2>/dev/null
exit 0
