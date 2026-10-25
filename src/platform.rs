use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    path::{Component, Path, PathBuf},
};

pub fn command_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::{
            ffi::OsString,
            os::windows::ffi::{OsStrExt, OsStringExt},
            path::Prefix,
        };
        let text: Vec<_> = path.as_os_str().encode_wide().collect();
        if let Some(Component::Prefix(prefix)) = path.components().next() {
            match prefix.kind() {
                Prefix::VerbatimDisk(_) => return PathBuf::from(OsString::from_wide(&text[4..])),
                Prefix::VerbatimUNC(_, _) => {
                    let mut unc = vec![b'\\' as u16, b'\\' as u16];
                    unc.extend_from_slice(&text[8..]);
                    return PathBuf::from(OsString::from_wide(&unc));
                }
                _ => {}
            }
        }
    }
    path.to_path_buf()
}

pub fn relative_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for part in path.components() {
        let Component::Normal(part) = part else {
            bail!("unsafe relative path: {}", path.display())
        };
        parts.push(part.to_str().context("path is not UTF-8")?);
    }
    let path = parts.join("/");
    filesystem_relative(&path)?;
    Ok(path)
}

pub fn filesystem_relative(path: &str) -> Result<()> {
    crate::model::safe_relative(path)?;
    #[cfg(windows)]
    crate::model::windows_relative(path)?;
    Ok(())
}

pub fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(unix)]
    {
        metadata.file_type().is_symlink()
    }
}

pub fn mode(metadata: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }
    #[cfg(windows)]
    {
        if metadata.permissions().readonly() {
            0o444
        } else {
            0o644
        }
    }
}

pub fn set_mode(file: &File, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
    }
    #[cfg(windows)]
    {
        let mut permissions = file.metadata()?.permissions();
        permissions.set_readonly(mode & 0o200 == 0);
        file.set_permissions(permissions)?;
    }
    Ok(())
}

pub fn open_input(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };
        // Keep the snapshot source stable while it is copied; never follow a junction/symlink.
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && !is_link(&metadata),
        "input must be a regular file, not a link"
    );
    Ok(file)
}

pub fn unchanged(file: &File, before: &Metadata, path: &Path, size: u64) -> Result<bool> {
    let after = file.metadata()?;
    let current = fs::symlink_metadata(path)?;
    if size != before.len() || !current.is_file() || is_link(&current) {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ino() == current.ino()
            && before.dev() == current.dev()
            && before.len() == current.len())
    }
    #[cfg(windows)]
    {
        Ok(before.modified()? == after.modified()?
            && before.len() == current.len()
            && windows::identity(file)? == windows::identity(&open_input(path)?)?)
    }
}

pub fn private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        fs::create_dir_all(path)?;
        let metadata = fs::metadata(path)?;
        ensure!(
            metadata.is_dir(),
            "private path must be a directory: {}",
            path.display()
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "private directory must belong to the current user: {}",
            path.display()
        );
        if metadata.mode() & 0o7777 != 0o700 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(windows)]
    windows::private_dir(path)?;
    Ok(())
}

pub fn create_private_file(path: &Path) -> Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?)
    }
    #[cfg(windows)]
    windows::create_private_file(path)
}

pub fn protect_file(file: &File) -> Result<()> {
    ensure!(
        !is_link(&file.metadata()?),
        "private path must not be a link"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            file.metadata()?.nlink() == 1,
            "private file must not have hard links"
        );
        set_mode(file, 0o600)?;
    }
    #[cfg(windows)]
    windows::protect_file(file)?;
    Ok(())
}

pub fn read_private_file(path: &Path) -> Result<File> {
    let file =
        open_input(path).context("read token file; run ylcc init or provide --token-file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            file.metadata()?.permissions().mode() & 0o077 == 0,
            "token file must be owner-only (chmod 600)"
        );
    }
    #[cfg(windows)]
    windows::check_private(&file).context("token file ACL must allow only its owner, SYSTEM and Administrators; use ylcc init or the installer")?;
    Ok(file)
}

pub fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::rename(source, destination)?;
    #[cfg(windows)]
    windows::replace_file(source, destination)?;
    Ok(())
}

/// Fraction (0..=1) of the filesystem holding `path` that is in use, as `df` reports it.
pub fn disk_usage(path: &Path) -> Result<f64> {
    let (used, total) = disk_space(path)?;
    Ok(used as f64 / total as f64)
}

/// Used and total bytes on the filesystem holding `path`, using space available to this user.
pub fn disk_space(path: &Path) -> Result<(u64, u64)> {
    #[cfg(unix)]
    let (available, total) = {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == 0,
            "statvfs: {}",
            std::io::Error::last_os_error()
        );
        (stat.f_bavail as u64 * stat.f_frsize as u64, stat.f_blocks as u64 * stat.f_frsize as u64)
    };
    #[cfg(windows)]
    let (available, total) = windows::disk_space(path)?;
    ensure!(total > 0, "filesystem reports no capacity");
    Ok((total.saturating_sub(available), total))
}

pub fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(windows)]
    let _ = path; // MoveFileExW uses WRITE_THROUGH; Windows does not expose directory fsync.
    Ok(())
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        ffi::c_void,
        mem::{size_of, zeroed},
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree},
        Security::{Authorization::*, *},
        Storage::FileSystem::*,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    struct LocalMemory(*mut c_void);
    impl Drop for LocalMemory {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    struct User(Vec<usize>);
    impl User {
        fn current() -> Result<Self> {
            let mut token = null_mut();
            ensure!(
                unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } != 0,
                "open current user token: {}",
                std::io::Error::last_os_error()
            );
            let token = unsafe { OwnedHandle::from_raw_handle(token) };
            let mut size = 0;
            unsafe {
                GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size);
            }
            ensure!(size > 0, "read current user SID size");
            let mut user = Self(vec![0; (size as usize).div_ceil(size_of::<usize>())]);
            ensure!(
                unsafe {
                    GetTokenInformation(
                        token.as_raw_handle(),
                        TokenUser,
                        user.0.as_mut_ptr().cast(),
                        size,
                        &mut size,
                    )
                } != 0,
                "read current user SID: {}",
                std::io::Error::last_os_error()
            );
            Ok(user)
        }
        fn sid(&self) -> PSID {
            unsafe { (*(self.0.as_ptr().cast::<TOKEN_USER>())).User.Sid }
        }
        fn text(&self) -> Result<String> {
            sid_text(self.sid())
        }
    }
    fn sid_text(sid: PSID) -> Result<String> {
        ensure!(!sid.is_null(), "SID is null");
        let mut text = null_mut();
        ensure!(
            unsafe { ConvertSidToStringSidW(sid, &mut text) } != 0,
            "format SID: {}",
            std::io::Error::last_os_error()
        );
        let _memory = LocalMemory(text.cast());
        let mut len = 0;
        unsafe {
            while *text.add(len) != 0 {
                len += 1;
            }
        }
        Ok(String::from_utf16(unsafe {
            std::slice::from_raw_parts(text, len)
        })?)
    }
    fn wide(path: &Path) -> Result<Vec<u16>> {
        let mut text: Vec<_> = path.as_os_str().encode_wide().collect();
        ensure!(!text.contains(&0), "path contains NUL");
        text.push(0);
        Ok(text)
    }
    fn descriptor() -> Result<LocalMemory> {
        let sid = User::current()?.text()?;
        let text: Vec<_> =
            format!("O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)\0")
                .encode_utf16()
                .collect();
        let mut descriptor = null_mut();
        ensure!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            } != 0,
            "create private ACL: {}",
            std::io::Error::last_os_error()
        );
        Ok(LocalMemory(descriptor))
    }
    fn attributes(descriptor: &LocalMemory) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        }
    }
    fn security(file: &File) -> Result<(LocalMemory, PSID, *mut ACL)> {
        let mut owner = null_mut();
        let mut acl = null_mut();
        let mut descriptor = null_mut();
        let code = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor,
            )
        };
        ensure!(
            code == 0,
            "read file ACL: {}",
            std::io::Error::from_raw_os_error(code as i32)
        );
        Ok((LocalMemory(descriptor), owner, acl))
    }
    fn trusted(sid: PSID, user: &User) -> bool {
        !sid.is_null()
            && unsafe {
                EqualSid(sid, user.sid()) != 0
                    || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                    || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
            }
    }
    fn is_hyperv_container_sid(sid: &str) -> bool {
        let parts: Vec<_> = sid.split('-').collect();
        parts.len() == 9
            && parts[0] == "S"
            && parts[1] == "1"
            && parts[2] == "5"
            && parts[3] == "83"
            && parts[4] == "1"
            && parts[5..].iter().all(|part| part.parse::<u32>().is_ok())
    }
    fn trusted_owner(sid: PSID, user: &User) -> bool {
        if trusted(sid, user) {
            return true;
        }
        // Hyper-V assigns this VM SID as the owner of volume objects. Accept it only
        // in the container image; it is never accepted as a token ACL grantee.
        std::env::var("YLCC_CONTAINERIZED").is_ok_and(|value| value == "1")
            && sid_text(sid).is_ok_and(|sid| is_hyperv_container_sid(&sid))
    }
    fn owned(file: &File) -> Result<()> {
        let (_descriptor, owner, _) = security(file)?;
        ensure!(
            trusted_owner(owner, &User::current()?),
            "private path must belong to the current user, SYSTEM or Administrators"
        );
        Ok(())
    }
    pub(super) fn check_private(file: &File) -> Result<()> {
        let (_descriptor, owner, acl) = security(file)?;
        let user = User::current()?;
        ensure!(
            trusted_owner(owner, &user),
            "token owner is not the current user, SYSTEM or Administrators"
        );
        ensure!(!acl.is_null(), "token has an unrestricted ACL");
        for index in 0..unsafe { (*acl).AceCount } as u32 {
            let mut ace = null_mut();
            ensure!(
                unsafe { GetAce(acl, index, &mut ace) } != 0,
                "read token ACE"
            );
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 || header.AceType == 1 {
                continue;
            }
            ensure!(
                header.AceType == 0,
                "token has an unsupported access-granting ACE"
            );
            let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            let sid = std::ptr::addr_of!(allowed.SidStart) as PSID;
            ensure!(
                trusted(sid, &user),
                "token ACL grants another user or group access"
            );
        }
        Ok(())
    }
    pub(super) fn protect_file(file: &File) -> Result<()> {
        let mut info = unsafe { zeroed::<BY_HANDLE_FILE_INFORMATION>() };
        ensure!(
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
            "inspect private file: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "private path must not be a reparse point"
        );
        ensure!(
            info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 || info.nNumberOfLinks == 1,
            "private file must not have hard links"
        );
        let handle = unsafe {
            ReOpenFile(
                file.as_raw_handle(),
                READ_CONTROL | WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        };
        ensure!(
            handle != INVALID_HANDLE_VALUE,
            "open file for ACL protection: {}",
            std::io::Error::last_os_error()
        );
        let handle = unsafe { File::from_raw_handle(handle) };
        protect_acl(&handle)
    }
    fn protect_acl(file: &File) -> Result<()> {
        owned(file)?;
        let descriptor = descriptor()?;
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = null_mut();
        ensure!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
            } != 0
                && present != 0,
            "read private ACL"
        );
        let code = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl,
                null(),
            )
        };
        ensure!(
            code == 0,
            "protect file ACL: {}",
            std::io::Error::from_raw_os_error(code as i32)
        );
        check_private(file)
    }
    pub(super) fn private_dir(path: &Path) -> Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty() && !p.exists())
        {
            private_dir(parent)?;
        }
        if !path.exists() {
            let descriptor = descriptor()?;
            ensure!(
                unsafe { CreateDirectoryW(wide(path)?.as_ptr(), &attributes(&descriptor)) } != 0,
                "create private directory {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        let mut options = OpenOptions::new();
        use std::os::windows::fs::OpenOptionsExt;
        options
            .access_mode(READ_CONTROL | WRITE_DAC)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(path)?;
        ensure!(
            file.metadata()?.is_dir() && !is_link(&file.metadata()?),
            "private directory must not be a reparse point"
        );
        // The directory handle already has ACL access; ReOpenFile rejects this directory handle.
        protect_acl(&file)
    }
    pub(super) fn create_private_file(path: &Path) -> Result<File> {
        let descriptor = descriptor()?;
        let handle = unsafe {
            CreateFileW(
                wide(path)?.as_ptr(),
                GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC,
                FILE_SHARE_READ,
                &attributes(&descriptor),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        ensure!(
            handle != INVALID_HANDLE_VALUE,
            "create private file {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        Ok(unsafe { File::from_raw_handle(handle) })
    }
    pub(super) fn identity(file: &File) -> Result<(u32, u32, u32)> {
        let mut info = unsafe { zeroed::<BY_HANDLE_FILE_INFORMATION>() };
        ensure!(
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
            "read file identity: {}",
            std::io::Error::last_os_error()
        );
        Ok((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }
    pub(super) fn disk_space(path: &Path) -> Result<(u64, u64)> {
        let (mut available, mut total) = (0u64, 0u64);
        ensure!(
            unsafe { GetDiskFreeSpaceExW(wide(path)?.as_ptr(), &mut available, &mut total, null_mut()) } != 0,
            "disk space of {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        Ok((available, total))
    }
    pub(super) fn replace_file(source: &Path, destination: &Path) -> Result<()> {
        ensure!(
            unsafe {
                MoveFileExW(
                    wide(source)?.as_ptr(),
                    wide(destination)?.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } != 0,
            "replace file {}: {}",
            destination.display(),
            std::io::Error::last_os_error()
        );
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::is_hyperv_container_sid;

        #[test]
        fn recognizes_only_canonical_hyperv_container_owner_sids() {
            assert!(is_hyperv_container_sid(
                "S-1-5-83-1-4041128834-1186162995-2597271713-775289774"
            ));
            for sid in [
                "S-1-5-83-0",
                "S-1-5-83-1-1-2-3",
                "S-1-5-83-1-1-2-3-4-5",
                "S-1-5-82-1-1-2-3-4",
                "S-1-5-83-2-1-2-3-4",
                "S-1-5-83-1-1-2-3-4294967296",
            ] {
                assert!(!is_hyperv_container_sid(sid), "accepted {sid}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_filesystem_rules_follow_the_host() {
        for path in ["aux.txt", "con/part.rs", "logs/2026-10-16T11:22:33.log"] {
            assert_eq!(filesystem_relative(path).is_err(), cfg!(windows));
            assert_eq!(relative_path(Path::new(path)).is_err(), cfg!(windows));
        }
        assert!(filesystem_relative("../escape").is_err());
    }
    #[test]
    fn private_files_refuse_replacement_and_retain_private_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let file = create_private_file(&path).unwrap();
        protect_file(&file).unwrap();
        drop(file);
        read_private_file(&path).unwrap();
        assert!(create_private_file(&path).is_err());
    }
    #[test]
    #[cfg(windows)]
    fn private_directories_preserve_data_and_protect_acl_on_reuse() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, READ_CONTROL};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private");
        private_dir(&path).unwrap();
        fs::write(path.join("keep"), "data").unwrap();
        private_dir(&path).unwrap();
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&path)
            .unwrap();
        windows::check_private(&file).unwrap();
        assert_eq!(fs::read_to_string(path.join("keep")).unwrap(), "data");
    }
    #[test]
    fn replacement_updates_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("new");
        let destination = dir.path().join("old");
        fs::write(&source, "new").unwrap();
        fs::write(&destination, "old").unwrap();
        replace_file(&source, &destination).unwrap();
        assert_eq!(fs::read_to_string(destination).unwrap(), "new");
        assert!(!source.exists());
    }
    #[test]
    #[cfg(windows)]
    fn public_token_aces_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        drop(create_private_file(&path).unwrap());
        assert!(
            std::process::Command::new("icacls.exe")
                .arg(&path)
                .args(["/grant", "*S-1-1-0:(R)"])
                .status()
                .unwrap()
                .success()
        );
        assert!(read_private_file(&path).is_err());
    }
    #[test]
    #[cfg(unix)]
    fn public_token_mode_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        fs::write(&path, "token").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private_file(&path).is_err());
    }
}
