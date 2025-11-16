/**
 * Depth and visual enhancements
 * Includes hex patterns, vignettes, wireframes, and crate card interactions
 */

export function initDepthEnhancements() {
    initHexPattern();
    initVignette();
    initArchitectureWireframe();
    initCrateCardInteractions();
}

function initHexPattern() {
    const hero = document.getElementById('hero');
    if (hero) {
        const hexPattern = document.createElement('div');
        hexPattern.className = 'hex-pattern absolute inset-0 opacity-5';
        hero.appendChild(hexPattern);

        gsap.to(hexPattern, {
            y: -100,
            scrollTrigger: {
                trigger: hero,
                start: 'top top',
                end: 'bottom top',
                scrub: true
            }
        });
    }
}

function initVignette() {
    const vignette = document.createElement('div');
    vignette.className = 'fixed inset-0 pointer-events-none z-0';
    vignette.style.cssText = `
        background: radial-gradient(ellipse at center, transparent 0%, rgba(0,0,0,0.1) 100%);
    `;
    document.body.appendChild(vignette);

    gsap.to(vignette, {
        opacity: 0.14,
        duration: 3,
        repeat: -1,
        yoyo: true,
        ease: 'sine.inOut'
    });
}

function initArchitectureWireframe() {
    const architectureSection = document.getElementById('architecture');
    if (architectureSection) {
        const wireframe = document.createElement('div');
        wireframe.className = 'absolute inset-0 opacity-5';
        wireframe.style.cssText = `
            background-image: 
                linear-gradient(rgba(211,117,43,0.1) 1px, transparent 1px),
                linear-gradient(90deg, rgba(211,117,43,0.1) 1px, transparent 1px);
            background-size: 50px 50px;
        `;
        architectureSection.querySelector('#architecture-container').appendChild(wireframe);

        gsap.fromTo(wireframe,
            { opacity: 0 },
            {
                opacity: 0.05,
                scrollTrigger: {
                    trigger: architectureSection,
                    start: 'top 80%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }
}

function initCrateCardInteractions() {
    const crateCards = document.querySelectorAll('.crate-card');
    crateCards.forEach((card, index) => {
        card.addEventListener('mouseenter', () => {
            gsap.to(card, {
                scale: 1.05,
                duration: 0.3,
                ease: 'power2.out',
                force3D: true
            });

            // Highlight connected crates
            crateCards.forEach((otherCard, otherIndex) => {
                if (otherIndex !== index && Math.abs(otherIndex - index) <= 1) {
                    gsap.to(otherCard, {
                        borderColor: 'rgba(211,117,43,0.5)',
                        duration: 0.3
                    });
                }
            });
        });

        card.addEventListener('mouseleave', () => {
            gsap.to(card, {
                scale: 1,
                duration: 0.3,
                ease: 'power2.out'
            });

            crateCards.forEach((otherCard) => {
                gsap.to(otherCard, {
                    borderColor: '#1a1a1a',
                    duration: 0.3
                });
            });
        });
    });
}

