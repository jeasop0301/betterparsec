BetterParsec — G007 keyboard-capture install kit
=================================================

This installs the kernel keyboard-filter driver + LocalSystem broker needed for
the real Alt+Tab capture path (G007 cell C5). No PowerShell required on this
machine: the installer is a native UAC-elevated program, like the Parsec setup.

Files in this folder:
  betterparsec-installer.exe   the installer (double-click it)
  betterparsec-kbdflt.sys      the keyboard-filter driver (test-signed)
  betterparsec-kbdflt.cer      the driver's test certificate
  input-broker.exe             the LocalSystem input broker

Keep all four files together in the same folder.

--------------------------------------------------------------------------------
BEFORE YOU START — one BIOS requirement
--------------------------------------------------------------------------------
Our driver is TEST-signed, so Windows will only load it when:
  * Secure Boot is OFF (a BIOS/UEFI setting), and
  * test-signing mode is ON (the installer turns this on for you).

If Secure Boot is ON, the installer will stop and tell you. Disable Secure Boot
in the BIOS first, then re-run it. (A Microsoft-signed build removes this
requirement — see the note at the bottom.)

To check first without changing anything:
  Right-click betterparsec-installer.exe -> Run as administrator, then in the
  window type:  status   (or run it and it will show install status)

--------------------------------------------------------------------------------
INSTALL
--------------------------------------------------------------------------------
1. Double-click betterparsec-installer.exe.
2. Accept the UAC prompt (this is what replaces the blocked PowerShell).
3. It trusts the cert, enables test-signing, installs the driver + broker service,
   and registers the keyboard filter. Follow the prompt to REBOOT.

AFTER REBOOT, confirm the capture path is live:
  * Service "BetterParsecInput" is Running.
  * The app log shows: keyboard capture armed through LocalSystem broker
Then run the G007 cells (see docs/LIVE-CHECKLIST.md), especially C5 with a REAL
physical Alt+Tab.

--------------------------------------------------------------------------------
UNINSTALL / RECOVERY
--------------------------------------------------------------------------------
Double-click, or from an elevated console:  betterparsec-installer.exe uninstall
Then reboot. The keyboard driver fails open locally; if anything misbehaves, a
reboot restores normal keyboard control.
To leave test mode entirely, run:  bcdedit /set testsigning off   (then reboot).

--------------------------------------------------------------------------------
NOTE — a Microsoft-signed driver removes the Secure Boot / test-mode steps
--------------------------------------------------------------------------------
Once the .sys is Microsoft attestation/WHQL-signed (production trust, G032), run:
  betterparsec-installer.exe --production-signed
It then installs with NO test-signing and Secure Boot left ON — exactly like the
Parsec installer.
