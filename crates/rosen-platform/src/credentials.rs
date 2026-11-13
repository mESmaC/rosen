//! Credential Manager encrypts generic credential blobs with user DPAPI.
use std::{io, ptr};
use windows_sys::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
    CRED_TYPE_GENERIC,
};
fn wide(text: &str) -> io::Result<Vec<u16>> {
    if text.contains('\0') {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(text.encode_utf16().chain(Some(0)).collect())
}
pub fn write(target: &str, secret: &[u8]) -> io::Result<()> {
    let target = wide(target)?;
    let size = u32::try_from(secret.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_ptr().cast_mut(),
        CredentialBlobSize: size,
        CredentialBlob: secret.as_ptr().cast_mut(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    // SAFETY: all pointers refer to live buffers for this synchronous call.
    if unsafe { CredWriteW(&credential, 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
pub fn read(target: &str) -> io::Result<Vec<u8>> {
    let target = wide(target)?;
    let mut credential: *mut CREDENTIALW = ptr::null_mut();
    // SAFETY: the API initializes an owned allocation on success only.
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CredReadW returns a valid credential and blob until CredFree.
    let bytes = unsafe {
        let value = &*credential;
        let bytes = if value.CredentialBlobSize == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(value.CredentialBlob, value.CredentialBlobSize as usize)
                .to_vec()
        };
        CredFree(credential.cast());
        bytes
    };
    Ok(bytes)
}

/// Removes an enrollment from the current user's vault.
pub fn delete(target: &str) -> io::Result<()> {
    let target = wide(target)?;
    // SAFETY: the terminated target buffer is live throughout the call.
    if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
