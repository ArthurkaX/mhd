// Quick check of available WASAPI types
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;

fn main() {
    // Just checking these types exist at compile time
    let _ = IMMDeviceEnumerator::default();
    let _ = IAudioClient::default();
    let _ = IAudioCaptureClient::default();
    let _ = WAVEFORMATEX::default();
    let _ = AUDCLNT_SHAREMODE_SHARED;
}
