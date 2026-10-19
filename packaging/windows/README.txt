Hop for Windows
===============

Hop shares one keyboard and mouse between this PC and your Mac. Text
you copy goes along when the cursor moves across, and files you drop
at the screen edge are sent to the other computer. Files from the Mac
arrive in Downloads\Hop Received, marked as downloaded, so Windows
checks them like any download.

Before you start it
-------------------
Keep Hop.exe in a folder it can stay in for good, for example
C:\Users\<your name>\Hop. Windows Firewall remembers Hop by this exact
location, so if Hop.exe moves later, Windows asks about it again.

Starting Hop
------------
Double-click Hop.exe.

- If Windows says "Windows protected your PC", click More info, then
  Run anyway. Windows says this about new programs that are not signed
  by a known publisher.
- When Windows asks whether to let Hop use networks, choose Private
  networks and click Allow. If you click Cancel, Windows blocks Hop
  without telling you. To fix that: open Windows Security > Firewall &
  network protection > Allow an app through firewall, click Change
  settings, tick Private next to Hop, and click OK. If Hop is not in
  the list, click Allow another app... and choose this Hop.exe.
- If Smart App Control blocks Hop, Hop cannot run until Smart App
  Control is turned off, because Hop is not signed yet. Step 3 of the
  install guide (see below) explains how, and what to weigh first.

Quitting Hop
------------
Close Hop's window. Hop stops sharing first and gives the keyboard and
mouse back, then quits.

If the keyboard and mouse ever seem stuck on the Mac, press
Ctrl + Alt + Shift + Esc (the left-hand keys) on this PC to bring them
back at once.

Hop's log
---------
The log is in %LOCALAPPDATA%\Hop\Logs (paste that into File Explorer's
address bar), or click "Open log folder" in Hop. It never contains what
you type or copy, file names, or pairing codes.

The install guide
-----------------
Open https://github.com/KrishSChavan/hop/releases in a web browser,
signed in to GitHub with your personal account (the repository is
private). Each release's notes link to the install guide,
docs/INSTALL.md, for that version.
