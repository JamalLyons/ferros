/**
 * Live debugger simulation
 * Types out Rust code with breakpoint indicators and ownership highlighting
 */

export function initDebuggerSimulation() {
    const debuggerSection = document.getElementById('debugger-simulation');
    if (!debuggerSection) return;

    const debuggerTitle = document.getElementById('debugger-title');
    if (debuggerTitle) {
        gsap.fromTo(debuggerTitle,
            { opacity: 0, y: 30 },
            {
                opacity: 1,
                y: 0,
                duration: 0.6,
                ease: 'power4.out',
                force3D: true,
                scrollTrigger: {
                    trigger: debuggerTitle,
                    start: 'top 90%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    const terminal = debuggerSection.querySelector('.debugger-terminal');
    if (!terminal) return;

    const codeLines = [
        { text: 'fn main() {', delay: 0 },
        { text: '    let mut vec = Vec::new();', delay: 600 },
        { text: '    vec.push(42);', delay: 900 },
        { text: '    let borrowed = &vec;', delay: 1200 },
        { text: '    println!("{:?}", borrowed);', delay: 1500 },
        { text: '}', delay: 1800 }
    ];

    let currentLine = 0;
    let isTyping = false;
    const codeDisplay = terminal.querySelector('.code-display');

    function typeNextLine() {
        if (!codeDisplay) return;

        if (currentLine >= codeLines.length) {
            currentLine = 0;
            codeDisplay.innerHTML = '';
        }

        const line = codeLines[currentLine];
        const lineEl = document.createElement('div');
        lineEl.className = 'code-line';

        // Add breakpoint indicator
        if (currentLine === 2) {
            const bp = document.createElement('span');
            bp.className = 'breakpoint-indicator text-[#d3752b] mr-2';
            bp.innerHTML = '●';
            lineEl.appendChild(bp);
        }

        const textNode = document.createTextNode(line.text);
        lineEl.appendChild(textNode);
        codeDisplay.appendChild(lineEl);

        // Highlight ownership/borrows
        if (line.text.includes('&')) {
            gsap.fromTo(lineEl,
                { backgroundColor: 'rgba(211,117,43,0.3)' },
                { backgroundColor: 'transparent', duration: 0.5, delay: 0.3, force3D: true }
            );
        }

        currentLine++;
        if (currentLine < codeLines.length) {
            setTimeout(typeNextLine, codeLines[currentLine].delay - (currentLine > 0 ? codeLines[currentLine - 1].delay : 0));
        } else {
            setTimeout(() => {
                codeDisplay.innerHTML = '';
                currentLine = 0;
                typeNextLine();
            }, 2500);
        }
    }

    ScrollTrigger.create({
        trigger: debuggerSection,
        start: 'top 80%',
        onEnter: () => {
            if (!isTyping) {
                isTyping = true;
                typeNextLine();
            }
        }
    });
}

