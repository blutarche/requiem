"""dmgbuild settings for the Requiem installer; scripts/make-dmg.sh passes the paths with -D.

The window size and icon positions must match scripts/make-dmg-background.swift.
"""
# dmgbuild runs this file with `defines` (the -D values) in scope.

volume_name = "Requiem"
format = "UDZO"
filesystem = "HFS+"

files = [(defines["app"], "Requiem.app")]
symlinks = {"Applications": "/Applications"}
icon = defines["icon"]
background = defines["background"]

show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
default_view = "icon-view"

window_rect = ((200, 120), (660, 420))
icon_size = 128
text_size = 13
# Icon centres, in points from the window's top-left.
icon_locations = {
    "Requiem.app": (180, 205),
    "Applications": (480, 205),
}
