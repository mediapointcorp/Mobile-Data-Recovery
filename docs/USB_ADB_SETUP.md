# USB / ADB setup (Windows, realme and other Android phones)

PhotoRecovery talks to phones only through **ADB** (Android Debug Bridge), which the owner must explicitly authorise on the phone. Nothing is installed on the phone, and nothing on it is changed.

## 1. Install platform-tools on the PC

See `WINDOWS_BUILD.md` step 4. Check the installation:

```powershell
adb version
```

## 2. Enable Developer options and USB debugging on the phone

Menu names differ between realme UI versions. These are the usual paths; check them on your own device.

| Step | realme UI 2.0–5.0 (Android 11–14+) | realme UI 1.0 / ColorOS 6–7 |
|---|---|---|
| Open the build number | Settings → About device → Version | Settings → About phone → Version |
| Enable Developer options | Tap **Build number** 7 times and enter the lock-screen PIN | Same |
| Turn on USB debugging | Settings → Additional settings → Developer options → **USB debugging** | Settings → Additional settings → Developer options → USB debugging |

Other Android phones: Settings → About phone → tap **Build number** 7 times, then Settings → System → Developer options → USB debugging.

## 3. Connect and authorise

1. Connect with a **data-capable** USB cable. Charge-only cables are a common cause of failure.
2. Unlock the phone. If a USB-mode prompt appears and the device is not detected, choose **File transfer**.
3. A dialog **"Allow USB debugging?"** shows the PC's RSA key fingerprint. Accept it. Only tick "Always allow" if this is your own trusted PC.
4. In PhotoRecovery, open **Android device → Refresh devices**. On the command line:

```powershell
adb devices -l
# R5CT1234ABC   device usb:1-1 product:RMX3363 model:RMX3363 ...
```

| State shown | Meaning / fix |
|---|---|
| `device` | Ready |
| `unauthorized` | Unlock the phone and accept the prompt. If no prompt appears: Developer options → **Revoke USB debugging authorisations**, then reconnect |
| `offline` | Reconnect the cable, or run `adb kill-server` then `adb devices` |
| not listed | Use a different cable or port. Install the Google USB Driver or the OEM driver (Device Manager shows an "Android" device with a warning icon). Enable USB debugging again after OS updates |

## 4. What ADB can and cannot do

PhotoRecovery shows these on the device-details panel. They are facts about Android, not limits of this tool:

- **Can:** read shared storage (`/sdcard` = `/storage/emulated/0`) and SD cards (`/storage/XXXX-XXXX`). This includes hidden folders, `DCIM/.thumbnails`, app media under `Android/media`, and Android 11+ **system-trash** items (`.trashed-<expiry>-<name>`) that the Gallery's "Recently deleted" still holds.
- **Cannot (without root):** read raw storage blocks, other apps' private data (`/data/data`, and on Android 11+ usually `Android/data`), or deleted files' contents. Over plain ADB, a deleted file is simply gone from the file listing.
- **Encryption:** since Android 10, every new device uses **file-based encryption** (FBE) for internal storage. Deleted file contents remain only as ciphertext, and the keys are bound to the device and the user's credentials. **No tool can recover deleted internal-storage photos from an FBE device without those keys.** SD cards formatted as *portable* storage are not encrypted. They can be imaged with a card reader and scanned as a raw image, which is often the best way to recover deleted photos.

## 5. Recommended workflow for a phone

1. **Stop using the phone** for anything that writes data. New photos, app updates and even caches can overwrite deleted data.
2. Create a case folder on a PC drive with free space. It must not be on the phone or the SD card.
3. Run **Copy accessible photos**. This collects existing originals, the system trash, thumbnail caches and app media.
4. If the phone has a portable SD card: power off, remove the card, image it with a card reader and imaging software (e.g. `dd`, FTK Imager), then scan the image under **Storage image**.
5. Rooted *test* devices only: see `ACQUISITION.md`.

## Wireless debugging

Android 11+ supports wireless ADB (`adb pair`, then `adb connect host:port`). Serials of the form `192.168.x.y:port` are accepted. USB is recommended for speed and reliability.
