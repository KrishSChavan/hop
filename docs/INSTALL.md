# Installing and using Hop

Hop lets one keyboard and mouse control both your Mac and your Windows PC. You install it once on each computer and pair the two computers once. After that, you open Hop on both and click **Start sharing**.

You need:
- both computers on the same home Wi-Fi;
- a Mac with Apple silicon (M1 or later);
- about 15 minutes.

## 1. Download Hop

Do this on each computer.

1. Open <https://github.com/KrishSChavan/hop/releases> in a web browser. The repository is private, so sign in to GitHub first, with your personal account. A work account cannot see Hop.
2. The newest release is at the top. Under **Assets**, click the file for this computer:
   - on the Mac: **Hop-macOS.zip**;
   - on the PC: **Hop-Windows.zip**.

   You don't need the "Source code" files.
3. If the browser warns that the file "isn't commonly downloaded", keep it. The warning only means that few people download Hop, which is true of a personal app.
   - In Microsoft Edge: point at the download, click **…** (More actions), then **Keep**. If Edge asks again, click **Show more**, then **Keep anyway**.
   - In Chrome: click **Keep**.
4. Use the same release on both computers.

## 2. Set up the Mac

1. Open your Downloads folder in Finder. If you see **Hop-macOS.zip**, double-click it. You get **Hop** (Hop.app), with Hop's icon.
2. Drag Hop into the **Applications** folder in Finder's sidebar.
   - If the Mac does not let you (a work Mac may not), make a folder named Applications in your home folder, and put Hop there. In Finder, choose Go → Home, then File → New Folder, and name it `Applications`.
3. Double-click Hop. The first time, macOS stops it and says it could not verify Hop. That happens because Hop is not notarized by Apple, which is normal for a personal app like Hop. Click **Done**.
4. Let Hop open. There are two ways, and you only need one.
   - **In System Settings:** open Apple menu → System Settings → Privacy & Security. Scroll down to the note that Hop was blocked, and click **Open Anyway**. Confirm with your password or Touch ID, and click **Open Anyway** again if macOS asks once more. The button only shows for about an hour after macOS blocked Hop; if it is gone, double-click Hop again first.
   - **In Terminal**, if that button is missing (a work Mac can hide it): open Terminal (Applications → Utilities → Terminal), paste this line and press Return:
     ```
     xattr -dr com.apple.quarantine /Applications/Hop.app
     ```
     If you put Hop in the Applications folder inside your home folder, use this line instead:
     ```
     xattr -dr com.apple.quarantine ~/Applications/Hop.app
     ```
     Then double-click Hop again.

   You do this once for each new download of Hop.
5. Hop's window opens. Look at **Permissions**:
   - "Accessibility: Allowed", with a green tick: there is nothing to do. Earlier Hop test builds on this Mac were signed the same way, so macOS already trusts this one.
   - "Accessibility: Not allowed yet": click **Allow…**. macOS shows its prompt and opens System Settings → Privacy & Security → Accessibility. Switch **Hop** on. Hop notices by itself within a few seconds.

   Hop needs Accessibility to share this Mac's keyboard and mouse.
6. The first time Hop looks for the PC (when you pair or start sharing), macOS asks whether Hop may find devices on your local network. Click **Allow**.
   - If you clicked Don't Allow: open System Settings → Privacy & Security → Local Network, and switch Hop on.

## 3. Set up the PC

1. **Make your home Wi-Fi private.** Open Settings → Network & internet → Wi-Fi → your network → Network profile type, and choose **Private network**. On a Public network, Windows keeps the Mac from reaching Hop. ([WINDOWS-SETUP.md](WINDOWS-SETUP.md) has the same step.)
2. **Make a folder for Hop** that it can stay in for good. In File Explorer, type `%USERPROFILE%` in the address bar and press Enter. Right-click an empty spot, choose New → Folder, and name it `Hop`.
3. **Unzip Hop into it.** Open your Downloads folder. Right-click **Hop-Windows.zip** and choose **Extract All…**. Click **Browse…**, choose the Hop folder you just made, and click **Extract**.

   Keep Hop in that folder. Windows Firewall remembers Hop by its exact location, so if Hop.exe moves later, Windows asks about it again.
4. In the Hop folder, double-click **Hop.exe**. To find it again later, right-click Hop.exe and choose **Pin to Start**.
5. If Windows says "Windows protected your PC", click **More info**, then **Run anyway**. Windows says this about new programs that are not signed by a known publisher.
6. If a message says instead that **Smart App Control** blocked Hop, there is no Run anyway button. Smart App Control blocks programs that are not signed and that it does not know, and it cannot make an exception for one app. That includes a Hop built on the PC itself, so building it there does not help. Hop is not signed yet, so the only way to run it is to turn Smart App Control off: open Windows Security → App & browser control → Smart App Control settings, and choose **Off**.
   - You may not be able to turn Smart App Control back on later without reinstalling Windows, so think it over first. Microsoft Defender Antivirus keeps protecting the PC either way.
7. The first time Hop uses the network (when you pair or start sharing), Windows asks whether to let Hop use networks. Choose **Private networks** and click **Allow**. If Windows then asks whether to allow changes to your device, click **Yes**.
   - If you clicked Cancel, Windows blocks Hop without telling you. [If something goes wrong](#if-something-goes-wrong) shows how to fix it.

## 4. Pair the two computers

Pairing makes the two computers trust each other. You do it once.

The examples below use Hop's own names for the computers, "Mac" and "Windows PC". Yours may have other names.

1. Open Hop on both computers.
2. On the PC, click **Show a pairing code**. Hop shows a six-digit code, such as **482 913**. The code works once, for 2 minutes. Never show it in a screen share.
   - The PC shows the code because a work Mac may refuse connections that other computers start.
   - If Windows asks whether to let Hop use networks, choose **Private networks** and click **Allow**.
3. On the Mac, click **Enter a code**. Type the code into the **Code** field (spaces are fine), and click **Pair**.
   - If macOS asks about finding devices on your local network, click **Allow**, then click **Pair** again.
   - The Mac says "Looking for the computer showing the code…", then "Connecting…", then "Now click Yes, pair on Windows PC."
4. On the PC, Hop asks "Mac (macOS) entered the code. Pair with it?". Check that it names your Mac and that the Mac is waiting, then click **Yes, pair**.
5. Both computers say "Paired with" and the other computer's name.

If the Mac cannot find the PC:
- Type the PC's address into the **Address** field, under "If the other computer is not found", and click **Pair** again while the PC still shows the code. To find the PC's address, open Settings → Network & internet → Wi-Fi → your network's properties on the PC, and look for **IPv4 address**. It looks like 192.168.1.23.
- If that does not work either, swap: click **Show a pairing code** on the Mac and **Enter a code** on the PC.
- If the code runs out (after 2 minutes, or after 3 wrong tries), start again on both computers.

## 5. Say where each computer is

Hop needs to know which edge of the screen leads to the other computer.

1. On the Mac, under **Your other computer**, find **Where is it?**. Click the side the PC is on, as they sit on your desk: **Left**, **Right**, **Above** or **Below**.
2. On the PC, do the same for the Mac. It is always the opposite side. For example, if the PC is to the right of the Mac, choose **Right** on the Mac and **Left** on the PC. The tip under the buttons says which side to choose on the other computer.

Changes save at once.

## 6. Use Hop

The examples here are what the Mac shows when the PC is on its right.

1. Click **Start sharing** on both computers. The status line, next to the coloured dot, says "Looking for Windows PC…" and then "Connected to Windows PC."
2. Push the cursor past the edge Hop names: "Push the cursor past the right edge of this screen to move to Windows PC." The cursor carries on onto the other computer, and the keyboard follows. Hop says "Using Windows PC: this keyboard and mouse control it now."
3. To come back, move the cursor back the way it came, across the border between the two screens. Or just use the other computer's own trackpad, mouse or keyboard: that takes control back.
4. Keys stay as labelled. With the Mac's keyboard on the PC, use the PC's shortcuts: Ctrl+C copies and Ctrl+V pastes, and the Mac's Cmd key acts as the Windows key. With the PC's keyboard on the Mac, the Windows key acts as Cmd, so Windows key + C copies.

**The emergency keys.** If the keyboard and mouse ever seem stuck on the other computer, press these keys together on the computer you are typing on. Use the keys on the left side of the keyboard. Control comes back at once.
- On the Mac: **Control + Option + Shift + Escape**.
- On the PC: **Ctrl + Alt + Shift + Esc**.

**Stopping.** Click **Stop sharing**. Closing Hop's window also stops sharing: Hop gives the keyboard and mouse back first, then quits. On the Mac, Cmd+Q does the same.

Hop does not start by itself yet. Each time, open Hop and click **Start sharing** on both computers.

## 7. Update to a new version

1. Download the new zip on each computer, as in step 1, and keep it if the browser warns about it. Update both computers to the same version.
2. **On the Mac:** quit Hop (Cmd+Q). Unzip the new Hop and drag it into the same Applications folder as before, then click **Replace**. macOS blocks the new Hop's first open again, so use Open Anyway or the Terminal line from step 2.4. Accessibility stays allowed.
3. **On the PC:** close Hop. Extract the new zip into the same Hop folder as before, and choose **Replace the files in the destination**. Keep the same folder, so Windows Firewall still knows Hop. If Windows says "Windows protected your PC", click **More info**, then **Run anyway**.

Your pairing and your **Where is it?** choices stay as they are. Hop keeps them in its settings, not in the app.

## If something goes wrong

- **The status line says "Stopped: …".** The rest of the line says why. For example, another copy of Hop may already be running; quit it and click **Start sharing** again.
- **It keeps saying "Looking for …".**
  - Check that both computers are on the same Wi-Fi, and that sharing is on on both.
  - On the PC, check that the Wi-Fi is private (step 3.1) and that Hop is allowed through the firewall (next item).
  - A work VPN on the Mac can keep it from reaching the PC.
  - Type the other computer's address into **Address**, under **Your other computer**, and click **Save address**. Do this on both computers. The PC's address is found as in step 4. The Mac's is in System Settings → Wi-Fi: click **Details…** next to your network, then TCP/IP.
- **You clicked Cancel when Windows asked about networks.** Open Windows Security → Firewall & network protection → Allow an app through firewall. Click **Change settings**, find Hop, tick **Private**, and click **OK**. If Hop is not in the list, click **Allow another app…** and choose Hop.exe in your Hop folder.
- **Start sharing is greyed out.** Hop says "Pair with your other computer first." Pair them as in step 4.
- **To pair again**, for example with a new computer: click **Forget…** under **Your other computer**, then pair as in step 4.
- **Hop's log.** Click **Open log folder** under **Activity** in Hop's window. On the Mac the log is in `~/Library/Logs/Hop`, and on the PC in `%LOCALAPPDATA%\Hop\Logs`. The log never contains what you type or copy, or pairing codes, so you can share it with Claude.
