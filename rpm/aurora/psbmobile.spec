%global debug_package %{nil}
%global _build_id_links none
%global __os_install_post %{nil}
%global __provides_exclude_from ^/usr/share/io.github.denizomer10.PsbMobile/lib/.*$

AutoReqProv:    no
Name:           io.github.denizomer10.PsbMobile
Version:        0.2.1
Release:        1.aurora
Summary:        PSB Mobile through PenguDroid for Aurora OS
License:        Apache-2.0
URL:            https://github.com/denizomer10/PenguDroid
Source0:        psbmobile-aurora.tar.gz

%description
Standalone development build of PSB Mobile hosted by PenguDroid.
Includes the user-supplied APK and prepared ARM64 test libraries.
The Android runtime in Documents remains externally provisioned.

%prep
%setup -q -n pengudroid-aurora

%build
# Contains binaries cross-compiled by build-aurora-rpm.sh.

%install
mkdir -p %{buildroot}
cp -a usr %{buildroot}/

%files
/usr/bin/io.github.denizomer10.PsbMobile
/usr/share/io.github.denizomer10.PsbMobile/
/usr/share/applications/io.github.denizomer10.PsbMobile.desktop
/usr/share/icons/hicolor/*/apps/io.github.denizomer10.PsbMobile.png

%changelog
* Thu Nov 19 2026 PenguDroid contributors - 0.2.1-1.aurora
- Standalone PSB Mobile development application.
