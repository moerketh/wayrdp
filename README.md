<div align="center">

# wayrdp

**Enhanced Session for Wayland Linux guests on Hyper-V.**
Open VMConnect, pick a resolution, and get your real Wayland desktop at the size of the window.

![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)
![Rust 1.94+](https://img.shields.io/badge/rust-1.94%2B-orange.svg)
![FreeRDP 3](https://img.shields.io/badge/FreeRDP-3.x-6f42c1.svg)
![Hyprland](https://img.shields.io/badge/Hyprland-✓-58e1ff.svg)
![KDE Plasma 6.3+](https://img.shields.io/badge/KDE%20Plasma-6.3%2B-1d99f3.svg)
![GNOME 50+](https://img.shields.io/badge/GNOME-50%2B-4a86cf.svg)

[What it does](#what-it-does) · [Supported desktops](#supported-desktops) · [Get it](#get-it-through-vmcreate) · [Limitations](#limitations)

</div>

---

## What it does

xrdp only serves X11 desktops, and modern Linux desktops run on Wayland. wayrdp is an RDP server
built for Wayland. It attaches to the desktop that's already logged in and streams it to
VMConnect over Hyper-V sockets.

- 🖥️ **Your real session.** Same apps and windows as on the console, not a second login.
- 📐 **Fits the window.** You get a virtual screen at the window's size, and resizing the window
  resizes it.
- 🔌 **No network needed.** It runs over VMBus, so there are no firewall rules to set up.
- 🧠 **No GPU needed.** It runs entirely on the CPU, as Hyper-V guests do.

## Supported desktops

- **Hyprland** (Omarchy)
- **KDE Plasma 6.3+** (Kali, Parrot)
- **GNOME 50+** (Kali)

## Get it through VMCreate

[**VMCreate**](https://github.com/moerketh/VMCreate) creates Kali, Parrot, Omarchy and other
Linux VMs on Hyper-V in one click. For the supported desktops above, it installs and sets up
wayrdp for you. Leave the RDP backend on **Auto**, create the VM, and open it in VMConnect.

## Limitations

- **Lock screen:** the lock screen isn't streamed. VMCreate turns the idle lock off.
- **Keyboard layout:** on KDE and GNOME, keys follow the guest's keyboard layout, so set it to
  match your client.
- **Codec:** RemoteFX only, with no H.264, so text and gradients are less sharp than on a native
  Windows session.
- **Other compositors:** Sway and other wlroots compositors aren't supported.

## License

MIT. Built on [FreeRDP 3](https://www.freerdp.com/) (Apache-2.0).
