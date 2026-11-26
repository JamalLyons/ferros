/**
 * Demo frame.
 * The recorded session (#364) is not in the repo yet. This only reveals the
 * section title. The terminal body in index.html is the placeholder for that GIF.
 */

export function initDebuggerSimulation() {
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
}

