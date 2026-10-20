# How Ferry keeps your data safe

Ferry mirrors the source to the destination, including deletions, so its rules lean towards doing nothing when in doubt. The [README](../README.md) lists the short version.

## Copies

- **Verify before replace.** At the destination each file is written as `<name>.ferry-part`, read back and compared with the hash taken at the source, and only then renamed over the real file. An existing destination file is never touched until a verified copy is ready to replace it.
  - If the copy at the destination doesn't match, Ferry deletes the part file, keeps the laptop copy and tries again next trip.
  - If the laptop copy itself no longer matches what was carried, Ferry drops it and carries it again on the next source visit.
- **Resumable copies.** Partial files are kept and continued after a dropped network, a sleep or an unplugged cable, as long as the source file is unchanged. Before a partial file is continued, the part already on the laptop is compared with the same bytes read again from the source; if they differ, the copy starts over, so the hash is always the source's.
- **Unstable files are skipped.** Files modified in the last 10 minutes, or locked, wait for the next pass. While Ferry reads a source file, other Windows and SMB clients can read it but not write it, and a file someone has open for writing (a running VM disk or a database) is skipped until it is closed. Programs running on the NAS itself aren't bound by this; for those Ferry relies on the size and modified time not changing during the copy.
- **Size cap and free space.** One shared cap (default 50 GB) across all folder pairs, filled in your priority order, and Ferry always leaves at least 10 GB free on the laptop. A file larger than the cap is flagged once and skipped.

**What "verified" means.** The bytes read back from the destination share match the hash taken from the source. That proves the data reached the server intact. It doesn't prove the server wrote it to disk, because a NAS may answer from RAM.

## Deletes

- **Held deletes.** Deletes at the destination wait until everything new in that folder pair has landed and been verified. When a backup job swaps an old file for a new one, the destination keeps the old one until the new one has arrived.
- **Thresholds and confirmations.** If one pass would delete more than 10% of a folder's files, or more than 50 files, Ferry deletes nothing, sends a notification and lists the held deletes under Waiting for your OK. Small folders ask before every delete. The limits are `delete_confirm_percent` and `delete_confirm_count` in `config.toml`.
- **Hidden files aren't deleted.** Some servers leave files you can't read out of a listing (access-based enumeration, Samba's hide or veto settings). Before a delete is queued, Ferry looks the file up at the source by name; if it is still there, the delete is dropped.
- **The exact list you approved.** Confirming deletes approves the list you were shown. If it changed since (a fill ran in between), Ferry asks again.
- **Why deletes wait.** When a folder's deletes are held because a file can't be carried yet (too big, locked or still changing), Activity names the file.
- **No deletes from a bad listing.** Ferry deletes nothing when the source listing failed, was partial or was empty. A folder that is empty at the source is treated as suspicious unless you mark that pair as allowed to be empty. A renamed or missing source folder therefore deletes nothing at the destination.

## Folders and names

- **Skipped folders are left alone on both sides.** A folder pair can skip folders by name ("Skip folders named", for example `_repo`), at any depth. Ferry doesn't list, carry or delete anything inside them, at the source or at the destination, so turning a skip on never deletes the destination's copy. Files already carried from a folder that is now skipped are dropped from the laptop.
- **Folders can't overlap.** Two folder pairs can't share a destination or have one destination inside the other, because each would delete the other's files. Setup refuses it, and a trip skips such pairs if `config.toml` was edited by hand.
- **Names compare the way Windows compares them.** Ferry folds case like NTFS does, so `ß`, `ς` or the Kelvin sign don't make two names look different. Two source names that differ only in case (possible on a Linux server) can't both live in a Windows folder: Ferry carries neither and Activity names them.
- **Odd names from a server are refused.** A listing that contains a name with a path separator, a drive or stream colon, `..` or control characters is treated as unreadable, so nothing is copied to or deleted from outside the folder because of it.
- **A file that replaces a folder.** When a folder at the source became a file of the same name, the destination folder is in the way. If it holds only empty folders, Ferry removes it. Otherwise its files are deleted first, under the usual thresholds and confirmations, and the file lands in the same unload. Until then the folder stays and Activity says why.

- **Ferry's own files aren't mirrored.** `*.ferry-part` files and the `.ferry-id` identity file are never copied or deleted as your files. Part files left at the destination by an interrupted push are removed after the next unload of that folder.

## Networks

Ferry copies only over a path it can confirm stays on the local network, and before copying it checks that the server shows the identity file it left there on the first trip. See [What Ferry trusts](how-it-works.md#what-ferry-trusts) for what that does and doesn't stop.
