//! Handle-based classification for verified Windows removal.
use std::fs::File;
use std::io;

/// Returns the held object's reparse tag, or `None` for a plain object.
/// Open with FILE_FLAG_OPEN_REPARSE_POINT to query the link itself.
#[allow(unsafe_code)]
pub fn reparse_tag(file: &File) -> io::Result<Option<u32>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_TAG_INFO,
    };
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: File owns a live handle throughout this synchronous call.
    // The writable repr(C) buffer has the size and layout the API requires.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileAttributeTagInfo,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(info.ReparseTag))
}
