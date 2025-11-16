/**
 * Floating scroll navigation
 * Creates a vertical navigation bar on the right side of the page
 */

export function initFloatingNavigation() {
    const nav = document.createElement('div');
    nav.id = 'floating-nav';
    nav.className = 'fixed right-6 top-1/2 -translate-y-1/2 z-50 hidden md:flex flex-col gap-3';
    document.body.appendChild(nav);

    const sections = ['hero', 'different', 'debugger-simulation', 'comparison', 'features', 'architecture', 'mission', 'cta'];

    sections.forEach((sectionId, index) => {
        const dot = document.createElement('button');
        dot.className = 'nav-dot w-2 h-2 rounded-full bg-gray-600 transition-all duration-300';
        dot.setAttribute('data-section', sectionId);
        dot.setAttribute('aria-label', `Scroll to ${sectionId}`);

        dot.addEventListener('click', () => {
            const section = document.getElementById(sectionId);
            if (section) {
                section.scrollIntoView({
                    behavior: 'smooth',
                    block: 'start'
                });
            }
        });

        nav.appendChild(dot);
    });

    // Update active dot on scroll
    ScrollTrigger.create({
        trigger: 'body',
        start: 'top top',
        end: 'bottom bottom',
        onUpdate: (self) => {
            sections.forEach((sectionId, index) => {
                const section = document.getElementById(sectionId);
                const dot = nav.querySelector(`[data-section="${sectionId}"]`);
                if (section && dot) {
                    const rect = section.getBoundingClientRect();
                    const isVisible = rect.top < window.innerHeight / 2 && rect.bottom > window.innerHeight / 2;

                    if (isVisible) {
                        dot.classList.add('bg-[#d3752b]', 'w-3', 'h-3', 'shadow-[0_0_10px_#d3752b]');
                        dot.classList.remove('bg-gray-600', 'w-2', 'h-2');
                    } else {
                        dot.classList.remove('bg-[#d3752b]', 'w-3', 'h-3', 'shadow-[0_0_10px_#d3752b]');
                        dot.classList.add('bg-gray-600', 'w-2', 'h-2');
                    }
                }
            });
        }
    });
}

