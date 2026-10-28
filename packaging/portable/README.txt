lowlat host for Windows, portable

Nothing here is installed but the drivers, and each only where it is needed:
the display driver for a machine with no monitor lit, the input driver for
the host's own keyboard, mouse and pads. Everything runs from an elevated
PowerShell in this folder; a folder copied or unzipped from elsewhere needs
the policy bypass shown, or Unblock-File on the scripts first.

1. The display driver, once per machine (trusts the certificate it is signed
   with, then installs it; -Remove takes both back):

     powershell -ExecutionPolicy Bypass -File install-driver.ps1

   The input driver the same way, if wanted; the host types and points
   through it only when started with --virtual-input, and presents a guest's
   DualShock 4 or DualSense as that pad on it only with --virtual-pads:

     powershell -ExecutionPolicy Bypass -File install-input-driver.ps1

2. One display, kept until removed (`lowlat-vdd.exe` alone lists its
   commands; `lowlat-vdd.exe remove 0` unplugs it):

     .\lowlat-vdd.exe add 1920x1080@60

3. The host: asks for the Parsec account the first time (Python 3 needed),
   then starts lowlatd as the system account in a window of its own. Its
   other arguments are the host's flags, --name "Office PC" for one.
   --approve-shared lets in an account the host is shared with that is not
   pre-approved; without it such a guest is told, after 30 s, that the host
   did not approve in time.

     powershell -ExecutionPolicy Bypass -File start-host.ps1

   -Login logs in again, -Logout revokes the session. The session is kept in
   lowlatd.env and the log in lowlatd.log, both here.

Gamepads need ViGEmBus installed.
