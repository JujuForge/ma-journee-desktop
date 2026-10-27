//! Windows Hello (PIN, empreinte, visage) via `UserConsentVerifier`. Les appels bloquent jusqu'a
//! la reponse de l'utilisateur : a executer hors du thread principal.

use windows::{
    Security::Credentials::UI::{UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability},
    Win32::{Foundation::HWND, System::WinRT::IUserConsentVerifierInterop},
    core::{HSTRING, factory},
};
use windows_future::IAsyncOperation;

/// Vrai si un moyen Windows Hello est configure sur ce compte (PIN au minimum).
pub fn is_available() -> bool {
    UserConsentVerifier::CheckAvailabilityAsync()
        .and_then(|op| op.join())
        .is_ok_and(|a| a == UserConsentVerifierAvailability::Available)
}

/// Affiche la demande Windows Hello rattachee a la fenetre `hwnd` (sinon elle s'ouvre derriere).
/// `Ok(false)` si l'utilisateur annule ou echoue.
pub fn verify(hwnd: isize, message: &str) -> Result<bool, String> {
    let interop = factory::<UserConsentVerifier, IUserConsentVerifierInterop>().map_err(|e| e.to_string())?;
    // Safety : `hwnd` est le handle d'une fenetre de l'app, vivante pendant l'appel.
    let op: IAsyncOperation<UserConsentVerificationResult> =
        unsafe { interop.RequestVerificationForWindowAsync(HWND(hwnd as *mut _), &HSTRING::from(message)) }
            .map_err(|e| e.to_string())?;
    let result = op.join().map_err(|e| e.to_string())?;
    Ok(result == UserConsentVerificationResult::Verified)
}
