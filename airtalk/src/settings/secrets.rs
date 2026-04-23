use anyhow::{Context, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_FLAGS,
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

pub(crate) fn store_secret(target: &str, value: &str) -> Result<()> {
    let mut target_w = widestring(target);
    let mut blob = value.as_bytes().to_vec();
    let cred = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target_w.as_mut_ptr()),
        Comment: PWSTR::null(),
        LastWritten: Default::default(),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: std::ptr::null_mut(),
        TargetAlias: PWSTR::null(),
        UserName: PWSTR::null(),
    };
    unsafe {
        CredWriteW(&cred, 0).ok().context("CredWriteW")?;
    }
    Ok(())
}

pub(crate) fn load_secret(target: &str) -> Result<Option<String>> {
    let target_w = widestring(target);
    let mut raw = std::ptr::null_mut();
    let found = unsafe {
        CredReadW(
            PCWSTR(target_w.as_ptr()),
            CRED_TYPE_GENERIC,
            Some(0),
            &mut raw,
        )
    };
    if found.is_err() {
        return Ok(None);
    }

    let value = unsafe {
        let cred = &*raw;
        let bytes =
            std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize);
        String::from_utf8(bytes.to_vec()).context("Credential Manager payload is not UTF-8")?
    };
    unsafe {
        CredFree(raw as _);
    }
    Ok(Some(value))
}

pub(crate) fn delete_secret(target: &str) -> Result<()> {
    let target_w = widestring(target);
    let res = unsafe { CredDeleteW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, Some(0)) };
    if res.is_err() {
        return Ok(());
    }
    Ok(())
}

fn widestring(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
