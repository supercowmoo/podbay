# Podbay

If you're reading this, it is not usable. I am rebuilding from a local prototype. The only thing in here is a base image.


A generic podman runtime that I personally use. It launches a disposable container around a codebase you want to work in.

The base image (`podbay-base`) is small and adaptable. It is Debian slim with apt essentials, mise, and non-root user (`dev`). Everything specific to you and your codebase arrives at runtime through mounts: the codebase is mounted at `/workspace`, personal tool config is mounted in read-only, and mise installs whatever the codebase and your profile asks for.

Anything that grants elevated privilege, such as additions to the image, can only be requested from user-owned space, never from within the container. This means that absent a kernel exploit, the only thing that can be modified from inside the container is the codebase and the optional repo cache (which has added layers of verification).

