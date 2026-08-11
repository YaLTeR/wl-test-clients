these are mostly vibe coded

- `dmabuf_y_invert`: press Space to toggle between normal and y_invert buffer, press 1-8 to set buffer transform
- `popup_background_effect`: xdg-popups with ext-background-effect blur
- `subsurface_no_geometry`: toplevel with ext-background-effect and no geometry, clicking spawns a subsurface that may extend outside, causing window bbox changes
- `layer_shell_popups`: layer surface with popups on left click and subsurface on right click, with ext-background-effect blur
- `blur_region_switch_half`: toplevel with ext-background-effect blur on one half, press Space to switch halves (only blur region changes, no buffer damage)
- `background_layer_frame_callbacks`: layer surface on the background layer split in two halves, the left half constantly damages and redraws every frame callback
- `fullscreen_on_disabled_output`: as soon as a wl_output global is removed, calls xdg_toplevel::set_fullscreen on that output
- `pointer_lock_position_hint`: locks the pointer and eagerly commits cursor position hints, left click toggles the lock
- `popup_desync_subsurface_frame_callbacks`: xdg-popup containing an animated desynchronized subsurface; only the subsurface requests frame callbacks and commits
