set(VCPKG_TARGET_ARCHITECTURE x64)
set(VCPKG_CRT_LINKAGE static)
set(VCPKG_LIBRARY_LINKAGE static)

# Release only - skip debug builds to save time
set(VCPKG_BUILD_TYPE release)
