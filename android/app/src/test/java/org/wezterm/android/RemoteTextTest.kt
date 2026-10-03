package org.wezterm.android

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.wezterm.android.TerminalInputConnection.Companion.charactersFrom
import org.wezterm.android.TerminalInputConnection.Companion.keptLength
import org.wezterm.android.TerminalInputConnection.Companion.remoteText
import org.wezterm.android.TerminalInputConnection.Companion.whole

class RemoteTextTest {
    /** The laptop's edit for `sent` and the IME's text: (Backspaces, typed text). */
    private fun edit(sent: String, text: String, composingStart: Int = -1): Pair<Int, String> {
        val target = remoteText(sent, text, composingStart)
        val kept = keptLength(sent, target)
        return charactersFrom(sent, kept) to target.substring(kept)
    }

    @Test
    fun recomposingSentTextChangesNothingUntilTheCompositionEnds() {
        assertEquals("abc re-marked composing", 0 to "", edit("abc", "abc", 0))
        assertEquals("abc recomposed as abd", 0 to "", edit("abc", "abd", 0))
        assertEquals("abc recomposed as abdoce", 0 to "", edit("abc", "abdoce", 0))
        assertEquals("only the last word recomposed", 0 to "", edit("ls -l", "ls -a", 3))
        assertEquals("the composition committed as abdoce", 1 to "doce", edit("abc", "abdoce"))
        assertEquals("the composition committed unchanged", 0 to "", edit("abc", "abc"))
    }

    @Test
    fun textCommittedBeforeALocalCompositionIsSentAndTheCompositionIsNot() {
        assertEquals("composing after sent text", 0 to "", edit("ab", "abni", 2))
        assertEquals("a commit and a composition in one batch", 0 to "x", edit("ab", "abxy", 3))
        assertEquals("a narrowed composition leaves its tail on the phone", 0 to "", edit("abe", "abehello", 3))
        assertEquals("and deleting that tail sends nothing", 0 to "", edit("abe", "abehel", 3))
        assertEquals("an edit of sent text before a composition waits for it", 0 to "", edit("abc", "bcd", 2))
        assertEquals("then applies whole", 3 to "bcd", edit("abc", "bcd"))
    }

    @Test
    fun deletionsEraseUserPerceivedCharacters() {
        assertEquals("one emoji, two UTF-16 units, one Backspace", 1 to "", edit("ab😀", "ab"))
        assertEquals("an accent removed: erase é, type e", 1 to "e", edit("abé", "abe"))
        assertEquals("a forward delete inside sent text retypes what follows", 2 to "l", edit("abehel", "abehl"))
        assertEquals("a code point deletion of an emoji", 1 to "", edit("x😀", "x"))
    }

    @Test
    fun loneSurrogatesAreNotWholeText() {
        assertTrue(whole("ab😀"))
        assertFalse(whole("\uD83D"))
        assertFalse(whole("\uDE00a"))
        assertFalse(whole("a\uD83Db"))
    }
}
