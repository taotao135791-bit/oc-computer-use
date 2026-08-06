#!/usr/bin/env bash
# Put the desktop in the settings-01 task's initial state: light appearance
# (AppleInterfaceStyle absent) so the task deterministically switches to
# dark and the criterion (AppleInterfaceStyle == Dark) is unambiguous.
set +e
defaults delete -g AppleInterfaceStyle 2>/dev/null
killall System Settings 2>/dev/null
exit 0
