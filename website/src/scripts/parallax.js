/**
 * Parallax effects for hero section
 * Optimized with requestAnimationFrame throttling
 */

let ticking = false;

export function initParallax() {
    window.addEventListener('scroll', () => {
        if (!ticking) {
            window.requestAnimationFrame(() => {
                const scrolled = window.pageYOffset;
                const hero = document.getElementById('hero');
                const particles = document.getElementById('particles');

                if (hero && scrolled < hero.offsetHeight) {
                    const parallaxSpeed = 0.3;
                    if (particles) {
                        gsap.set(particles, {
                            y: scrolled * parallaxSpeed,
                            force3D: true
                        });
                    }
                }
                ticking = false;
            });
            ticking = true;
        }
    });
}

