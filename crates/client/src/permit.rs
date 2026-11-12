//! Разрешение микрофона. `cpal` на macOS сам диалог не показывает.

#[cfg(target_os = "macos")]
pub fn ensure_microphone() -> Result<(), String> {
    use std::sync::mpsc;
    use std::time::Duration;

    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

    // SAFETY: константа Apple живёт всё время процесса.
    let media = unsafe { AVMediaTypeAudio }.ok_or_else(|| "нет доступа к микрофону".to_string())?;
    // SAFETY: запрос статуса не меняет устройство и не требует главного потока.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) };
    if status == AVAuthorizationStatus::Authorized {
        return Ok(());
    }
    if status != AVAuthorizationStatus::NotDetermined {
        return Err("нет доступа к микрофону".into());
    }

    let (tx, rx) = mpsc::sync_channel(1);
    let handler = RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    // SAFETY: тип медиа — аудио. Блок держим до ответа, система вызывает его позже.
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &handler);
    }
    match rx.recv_timeout(Duration::from_secs(180)) {
        Ok(true) => Ok(()),
        _ => Err("нет доступа к микрофону".into()),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_microphone() -> Result<(), String> {
    Ok(())
}
