# 3. Windows, workspaces and displays

## Tiling: windows side by side

Most of the time you want two or four windows next to each other, not
piled on top of one another. Umoja snaps windows into place for you.

![Halves and quarters of the screen with their keys: Super+Left, Super+Right, Super+keypad 7, Super+Up](images/tiling.svg)

**With the mouse**, drag a window by its title bar:

- to the **left or right edge** of the screen for half the screen
- into a **corner** for a quarter
- to the **top edge** to maximize it

A shaded area shows where the window will go before you let go.

**With the keyboard**:

| Keys | Where the window goes |
| --- | --- |
| **Super+Left** / **Super+Right** | Left or right half |
| **Super+Up** / **Super+Down** | Maximize, or restore |
| **Super+keypad 8** / **Super+keypad 2** | Top or bottom half |
| **Super+keypad 7**, **9**, **1**, **3** | The four quarters |

![Two windows tiled side by side: Files on the left and a text editor on the right](images/tiling-halves.jpg)

When two windows are tiled side by side, **drag the line between them** to
make one wider and the other narrower at the same time.

While you move a window it **snaps** to the edges of the screen and of other
windows when it gets close, and its title bar never goes under the top bar
or off the screen. Maximizing a tiled window and restoring it puts it back
in its tile.

### The layout picker

Drag a window to the top of the screen and pause there. A small card of
layouts opens, each one split into tiles. Drop the window on the
tile you want it in.

![A window tiled into the top-left quarter of the screen](images/tiling-quarter.jpg)

### Or just ask

Ask Umoja can arrange windows for you: "put the terminal on the left and
Firefox on the right", "maximize Files", "move this to the other monitor".

![The assistant tiled Files to the left and the text editor to the right](images/ai-tiling.jpg)

To bring back a whole arrangement of windows by name, use a
[setup](08-setups-and-automations.md#setups).

## Workspaces

Workspaces are like extra screens. Keep mail and chat on one, your work on
another and music on a third, and switch between them instead of hunting
through a pile of windows.

![Workspaces in a row: mail and web on the first, code on the second, music on the third, and an empty one at the end](images/workspaces.svg)

Umoja manages workspaces for you, as Ubuntu does: there is always one empty
workspace at the end, ready to use. An empty workspace in the middle goes
away by itself.

| Keys | What it does |
| --- | --- |
| **Super+Page Down** / **Super+Page Up** | Next or previous workspace (also **Ctrl+Alt+Right**/**Left**) |
| **Super+Shift+Page Down** / **Up** | Take the current window to the next or previous workspace |
| **Super+Home** / **Super+End** | First or last workspace |

The dots at the left of the top bar show which workspace you're on; scroll
over them to switch. In the **overview** (**Super**, or push the pointer
into the top-left corner when **Settings › Multitasking › Hot Corner** is
on), your workspaces are along the top:

- **Click** a workspace to go there. Clicking the one you're on closes the
  overview.
- **Drag a window** onto a workspace to move it there. The small windows
  inside the workspaces can be dragged too, and dropping a window inside a
  workspace puts it at that spot on the screen.
- **Drop a window between two workspaces** to make a new workspace there.
- **Drag an app from the dock** onto a workspace to open a new window of
  it there, or between two workspaces to open it on a new one.
- Use the **arrow keys** to pick a window and **Enter** to go to it.

![The overview on the second workspace, which has a calculator open](images/workspace-2.jpg)

You can also ask: "go to workspace 2", "move Files to workspace 3".

## Several monitors

Plug in a monitor and Umoja uses it straight away, with the dock on every
monitor.

- **Arrange your monitors** in **Settings › Displays**: drag them to match
  how they sit on your desk, choose the main one, and set each one's
  resolution, refresh rate, scale and rotation. A new monitor is placed to
  the right of the others until you change it.
- **Move a window to another monitor** with **Super+Shift+Left** or
  **Super+Shift+Right** (or up and down), by dragging it across, or from
  the window menu.
- Umoja **remembers** each window's size, position, workspace and monitor,
  so apps open where you left them, even after you log out.
- The **overview** shows on every monitor: the main one has the
  workspaces and the search, and each other monitor shows its own windows.
  Drag a window from one monitor's view to another's to move it there.
- **Super+P** switches between using the displays side by side, mirrored,
  or only the external or built-in one.

## When an app asks for you

When an app wants your attention while you're busy in another window (a
download finished, a chat came in), Umoja doesn't pull you away. Its dock
icon lights up and a notification says it's ready; click either to go to
it. An app that stops responding gets a dialog after a few seconds where
you can **Wait** or **Force Quit** it.

## Virtual machines and remote desktops

A virtual machine or remote desktop window that has grabbed the keyboard
(click inside it, or its grab key such as **Ctrl+Alt+G**) gets every key
while it has focus, **Super**, **Alt+Tab** and **Print Screen** included,
so they work inside the VM. Volume and brightness keys still work on your
computer. Press **Super+Escape** to take the keys back. This works for
Wayland viewers (GNOME Boxes, virt-manager, QEMU's GTK window) and for
X11 ones such as QEMU's SDL window, which quickemu uses, VirtualBox,
Remmina and VMware.

## Display scale and HiDPI

On a high-resolution screen everything can look tiny. Umoja picks a
sensible scale when you first log in, and you can change it in **Settings ›
Displays › Scale**, including fractional scales such as 125% or 150%. To
make only the text bigger, use **Settings › Accessibility › Large Text**,
or ask "make the text bigger".

## Apps from anywhere, and games

Umoja runs every kind of app: GNOME and GTK apps, Qt and KDE apps,
Electron apps like VS Code, Discord and Slack, snaps, Flatpaks and
AppImages. It also runs older X11 apps and games through Xwayland, which
starts by itself when an app needs it.

Games that run full screen get the screen to themselves. Notifications wait
until you leave full screen, so they don't pop up mid-game (see
[Automatic Do Not Disturb](07-everyday-ai.md#automatic-do-not-disturb)).

## Laptops with two graphics cards

Many laptops have an Intel or AMD graphics chip plus an NVIDIA card. Umoja
uses the built-in chip for the desktop and lets the NVIDIA card sleep to
save battery. A monitor plugged into a port that's wired to the NVIDIA card
works too, but needs a new login after you plug it in. If your laptop's
firmware has a graphics setting, choose **Hybrid**.
