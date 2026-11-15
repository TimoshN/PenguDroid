# Game controllers

PenguDroid reads game controllers directly from the kernel (`/dev/input/event*`,
readable for the seat's user through logind). Wayland has no gamepad protocol,
so this is the only path; controllers are not grabbed, other programs keep
seeing them, and they can be plugged in or out while an app runs.

## Default: a real Android controller

A control that the app's keymap does not bind reaches the app the way a
Bluetooth/USB controller does on a phone:

| Controller | Android |
|---|---|
| A / B / X / Y | `KEYCODE_BUTTON_A` / `B` / `X` / `Y` |
| LB / RB, LT / RT | `KEYCODE_BUTTON_L1` / `R1`, `L2` / `R2` (+ `AXIS_LTRIGGER` / `AXIS_RTRIGGER`, `AXIS_BRAKE` / `AXIS_GAS`) |
| Start / Select / Home, L3 / R3 | `KEYCODE_BUTTON_START` / `SELECT` / `MODE`, `THUMBL` / `THUMBR` |
| D-pad (hat) | `KEYCODE_DPAD_*` + `AXIS_HAT_X` / `AXIS_HAT_Y` |
| Left stick / right stick | `AXIS_X`/`AXIS_Y`, `AXIS_Z`/`AXIS_RZ` (`SOURCE_JOYSTICK`) |

Keys are sent with `SOURCE_GAMEPAD`, so games with controller support (Unity,
Unreal, SDL, Godot) pick them up without configuration.

## Keymap: controller to touch, keyboard or mouse

Controls can be bound in the per-app keymap next to keyboard keys
(`~/.config/pengudroid/keymaps/<package>.keymap`, edited in the manager):

```text
PAD_A        = tap 1100 600            # a touch
PAD_LS_UP    = stick 230 520 110 up    # the left stick walks a touch joystick
PAD_LS_DOWN  = stick 230 520 110 down
PAD_LS_LEFT  = stick 230 520 110 left
PAD_LS_RIGHT = stick 230 520 110 right
PAD_RT       = hold 1180 560           # triggers press past half travel
PAD_B        = key ESC                 # acts as a keyboard key (ESC is Back)
PAD_RS       = pointer 14              # the right stick moves a mouse cursor
PAD_X        = key MOUSE_LEFT          # ... and X clicks where it points
```

Names: `PAD_A B X Y LB RB LT RT SELECT (BACK) START HOME (GUIDE) L3 R3`,
`PAD_UP DOWN LEFT RIGHT` (d-pad), `PAD_LS_UP/DOWN/LEFT/RIGHT`,
`PAD_RS_UP/DOWN/LEFT/RIGHT` (stick directions, with hysteresis), and
`PAD_LS` / `PAD_RS` (a whole stick, for `pointer`). `key` takes any keymap key
name, including `MOUSE_LEFT`/`MOUSE_RIGHT`; the emitted key goes through the
keyboard path, so a key that is itself bound to a touch triggers that touch.

## Environment

* `PENGUDROID_GAMEPAD=mouse` — for apps without `PAD_*` bindings, use a
  desktop layout: left stick moves the cursor, A clicks, X right-clicks,
  B is Back, Start is Enter, Select is Tab, the d-pad is the arrow keys,
  LB/RB are Page Up/Down.
* `PENGUDROID_GAMEPAD=off` — ignore controllers.
* `PENGUDROID_TRACE_INPUT=1` — log every controller event the app receives.
