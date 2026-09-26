// Embed the per-artifact payload as the `inka` binary section, shared by
// `inka build` (the single-file artifact) and `inka desktop` (the per-app shim).
//
// The read side lives in `inka-format::locate_section`, which parses the host
// image's bytes directly so the launcher and the desktop shim stay free of the
// libsui dependency tree. Only this writer needs libsui, and it runs in the
// `inka` CLI which already depends on it.

/// Embed `payload` as the `inka` section of `image`, returning the new image.
///
/// Uses Deno's `libsui` so the embedding is format-correct per target: an ELF
/// `PT_NOTE` graft on Linux, a `.rsrc` resource on Windows, and a `__SUI`
/// segment on macOS. This is the same mechanism `libdenort` uses to carry its
/// standalone payload.
#[cfg(target_os = "linux")]
pub(crate) fn embed(image: &[u8], payload: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    libsui::Elf::new(image)
        .append(inka_format::SECTION_NAME, payload, &mut out)
        .map_err(|e| format!("cannot embed payload section: {e}"))?;
    Ok(out)
}

#[cfg(target_os = "windows")]
pub(crate) fn embed(image: &[u8], payload: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    libsui::PortableExecutable::from(image)
        .map_err(|e| format!("cannot parse PE image: {e}"))?
        .write_resource(inka_format::SECTION_NAME, payload.to_vec())
        .map_err(|e| format!("cannot embed payload resource: {e}"))?
        .build(&mut out)
        .map_err(|e| format!("cannot write PE image: {e}"))?;
    Ok(out)
}

#[cfg(target_os = "macos")]
pub(crate) fn embed(image: &[u8], payload: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    libsui::Macho::from(image.to_vec())
        .map_err(|e| format!("cannot parse Mach-O image: {e}"))?
        .write_section(inka_format::SECTION_NAME, payload.to_vec())
        .map_err(|e| format!("cannot embed payload section: {e}"))?
        .build(&mut out)
        .map_err(|e| format!("cannot write Mach-O image: {e}"))?;
    Ok(out)
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub(crate) fn embed(_image: &[u8], _payload: &[u8]) -> Result<Vec<u8>, String> {
    Err("inka artifacts are not supported on this platform".to_string())
}

/// Embed `icon` (PNG or ICO bytes) as a Windows PE icon resource, returning the
/// new image. Only PE images carry icons; unix ships a co-located PNG instead.
#[cfg(target_os = "windows")]
pub(crate) fn set_icon(image: &[u8], icon: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    libsui::PortableExecutable::from(image)
        .map_err(|e| format!("cannot parse PE image: {e}"))?
        .set_icon(icon)
        .map_err(|e| format!("cannot embed icon: {e}"))?
        .build(&mut out)
        .map_err(|e| format!("cannot write PE image: {e}"))?;
    Ok(out)
}
