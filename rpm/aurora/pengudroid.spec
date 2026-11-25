%global debug_package %{nil}
%global _build_id_links none
%global __os_install_post %{nil}
%global __provides_exclude_from ^/usr/share/io.github.denizomer10.MaxNext/lib/.*$

AutoReqProv:    no
Name:           io.github.denizomer10.MaxNext
Version:        0.2.1
Release:        94.aurora
Summary:        Experimental Android compatibility layer for Aurora OS
License:        Apache-2.0
URL:            https://github.com/denizomer10/PenguDroid
Source0:        pengudroid-aurora.tar.gz

%description
PenguDroid hosts Android applications as ordinary Linux processes.
This is an experimental AArch64/Aurora OS package. It does not include
Google's Android runtime, application packages, or a native-code translator.
The Android 17 ARM64 runtime must be prepared separately on a build host.
The GUI, CLI, and daemon are dispatched through one multi-call executable.
Regular/store validation still needs to be repeated for each target release.

%prep
%setup -q -n pengudroid-aurora

%build
# The source archive contains binaries cross-compiled by build-aurora-rpm.sh.

%install
mkdir -p %{buildroot}
cp -a usr %{buildroot}/

%files
/usr/bin/io.github.denizomer10.MaxNext
/usr/share/io.github.denizomer10.MaxNext/
/usr/share/applications/io.github.denizomer10.MaxNext.desktop
/usr/share/icons/hicolor/*/apps/io.github.denizomer10.MaxNext.png

%changelog
* Wed Nov 25 2026 PenguDroid contributors - 0.2.1-94.aurora
- Add the android_wlegl transport probe (`wlegl-probe`)
- Initial experimental AArch64 Aurora OS package.
