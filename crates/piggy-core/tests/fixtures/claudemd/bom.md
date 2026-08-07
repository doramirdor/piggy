docs/missing-from-bom.md is the very first thing in this file.

The byte order mark in front of it must be gone before any detector reads a
token, or that reference comes back with three invisible bytes glued to it.
