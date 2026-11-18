#!/bin/sh
# Use the NDK compiler selected by cibuildwheel and support 16 KiB page sizes.
exec "${CC:?cibuildwheel must set the Android compiler}" -Wl,-z,max-page-size=16384 "$@"
