"""A tiny terminal emulator: reconstruct a screen grid from the escape
sequences agsh emits, so interactive (PTY) tests can assert on what a user would
actually see. Handles the CSI cursor/erase subset agsh uses plus OSC sequences
(consumed and ignored — e.g. shell-integration marks and window titles)."""
import re


class Term:
    def __init__(self, rows=24, cols=80):
        self.rows, self.cols = rows, cols
        self.g = [[" "] * cols for _ in range(rows)]
        self.r = self.c = 0

    def _scroll(self):
        self.g.pop(0)
        self.g.append([" "] * self.cols)

    def _nl(self):
        self.r += 1
        if self.r >= self.rows:
            self.r = self.rows - 1
            self._scroll()

    def _put(self, ch):
        if self.c >= self.cols:
            self.c = 0
            self._nl()
        self.g[self.r][self.c] = ch
        self.c += 1

    def feed(self, data):
        i, n = 0, len(data)
        while i < n:
            ch = data[i]
            if ch == "\x1b":
                # OSC: ESC ] ... (BEL | ESC \) — consume and ignore.
                if data[i + 1 : i + 2] == "]":
                    j = i + 2
                    while j < n and data[j] != "\x07" and data[j : j + 2] != "\x1b\\":
                        j += 1
                    i = j + (2 if data[j : j + 2] == "\x1b\\" else 1)
                    continue
                m = re.match(r"\x1b\[([0-9;?]*)([A-Za-z])", data[i:])
                if m:
                    params, cmd = m.group(1), m.group(2)
                    i += m.end()
                    first = params.split(";")[0] if params else ""
                    num = int(first) if first.isdigit() else 1
                    if cmd == "A":
                        self.r = max(0, self.r - num)
                    elif cmd == "B":
                        self.r = min(self.rows - 1, self.r + num)
                    elif cmd == "C":
                        self.c = min(self.cols - 1, self.c + num)
                    elif cmd == "D":
                        self.c = max(0, self.c - num)
                    elif cmd == "H":
                        self.r = self.c = 0
                    elif cmd == "K":
                        for x in range(self.c, self.cols):
                            self.g[self.r][x] = " "
                    elif cmd == "J":
                        for x in range(self.c, self.cols):
                            self.g[self.r][x] = " "
                        for y in range(self.r + 1, self.rows):
                            self.g[y] = [" "] * self.cols
                    # 'm' (SGR colors) and others: ignored.
                    continue
                i += 1
                continue
            if ch == "\r":
                self.c = 0
            elif ch == "\n":
                self._nl()
            elif ch == "\x08":
                self.c = max(0, self.c - 1)
            elif ch == "\x07":
                pass
            else:
                self._put(ch)
            i += 1

    def screen(self):
        return "\n".join("".join(row).rstrip() for row in self.g)

    def text(self):
        """All non-empty screen lines joined by newlines (whitespace-trimmed)."""
        return "\n".join(l for l in (r.rstrip() for r in (("".join(row)) for row in self.g)) if l)
