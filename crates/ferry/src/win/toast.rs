//! Toasts for an unpackaged app: a Start Menu shortcut carrying AppUserModelID "Ferry", then
//! WinRT `ToastNotificationManager`. WinRT is only touched when a toast is shown.
use std::io;
use std::path::PathBuf;
use windows::core::{Interface, HSTRING, PWSTR};
use windows::Data::Xml::Dom::IXmlNode;
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemAlloc, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager, ToastTemplateType};

const AUMID: &str = "Ferry";

pub fn shortcut_path() -> io::Result<PathBuf> {
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| io::Error::other("APPDATA not set"))?;
    Ok(PathBuf::from(appdata).join("Microsoft").join("Windows").join("Start Menu").join("Programs").join("Ferry.lnk"))
}

/// Create the Start Menu `Ferry.lnk` (pointing at this exe, with the AUMID) unless it exists.
pub fn ensure_shortcut() -> io::Result<()> {
    let lnk = shortcut_path()?;
    if lnk.exists() {
        return Ok(());
    }
    let exe = HSTRING::from(std::env::current_exe()?.as_path());
    let id: Vec<u16> = AUMID.encode_utf16().chain([0]).collect();
    unsafe {
        // S_FALSE (already initialized) or a different mode both leave COM usable here.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&exe)?;
        // The value must be COM-allocated: the shell may coerce it in place (freeing ours), so
        // PropVariantClear releases whatever the variant holds afterwards.
        let buf = CoTaskMemAlloc(id.len() * 2) as *mut u16;
        if buf.is_null() {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        buf.copy_from_nonoverlapping(id.as_ptr(), id.len());
        let mut pv = PROPVARIANT::default();
        let inner = &mut pv.Anonymous.Anonymous;
        inner.vt = VT_LPWSTR;
        inner.Anonymous.pwszVal = PWSTR(buf);
        let set = link.cast::<IPropertyStore>().and_then(|store| {
            store.SetValue(&PKEY_AppUserModel_ID, &pv)?;
            store.Commit()
        });
        let _ = PropVariantClear(&mut pv);
        set?;
        link.cast::<IPersistFile>()?.Save(&HSTRING::from(lnk.as_path()), true)?;
    }
    Ok(())
}

/// Show a two-line toast. Needs the shortcut from `ensure_shortcut` to appear.
pub fn toast(title: &str, body: &str) -> windows::core::Result<()> {
    let xml = ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02)?;
    let texts = xml.GetElementsByTagName(&HSTRING::from("text"))?;
    for (i, s) in [title, body].into_iter().enumerate() {
        let node: IXmlNode = xml.CreateTextNode(&HSTRING::from(s))?.cast()?;
        texts.Item(i as u32)?.AppendChild(&node)?;
    }
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)
}
