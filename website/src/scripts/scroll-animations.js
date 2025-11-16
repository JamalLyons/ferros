/**
 * Scroll-triggered animations for sections, cards, and comparison elements
 * Uses ScrollTrigger.batch() for optimized performance
 */

export function initScrollAnimations() {
    // Batch animations for better performance
    const sections = [
        { id: 'different-title', y: 30 },
        { id: 'features-title', y: 30 },
        { id: 'architecture-title', y: 30 },
        { id: 'mission-content', y: 20 },
        { id: 'cta-content', y: 20 }
    ];

    sections.forEach(section => {
        const el = document.getElementById(section.id);
        if (el) {
            gsap.fromTo(el,
                { opacity: 0, y: section.y },
                {
                    opacity: 1,
                    y: 0,
                    duration: 0.6,
                    ease: 'power4.out',
                    force3D: true,
                    scrollTrigger: {
                        trigger: el,
                        start: 'top 90%',
                        toggleActions: 'play none none none'
                    }
                }
            );
        }
    });

    // Batch card animations
    const cardGroups = [
        { selector: '#different-cards > div', stagger: 0.05 },
        { selector: '[data-feature]', stagger: 0.05 },
        { selector: '.crate-card', stagger: 0.03 }
    ];

    cardGroups.forEach(group => {
        const cards = document.querySelectorAll(group.selector);
        if (cards.length > 0) {
            ScrollTrigger.batch(cards, {
                onEnter: (elements) => {
                    gsap.fromTo(elements,
                        { opacity: 0, y: 30, scale: 0.95 },
                        {
                            opacity: 1,
                            y: 0,
                            scale: 1,
                            duration: 0.5,
                            ease: 'back.out(1.4)',
                            stagger: group.stagger,
                            force3D: true
                        }
                    );
                },
                start: 'top 90%'
            });
        }
    });

    // Comparison section animations
    initComparisonAnimations();

    // Architecture arrows and dividers
    initArchitectureAnimations();
}

function initComparisonAnimations() {
    const comparisonLeft = document.getElementById('comparison-left');
    const comparisonRight = document.getElementById('comparison-right');

    if (comparisonLeft) {
        gsap.fromTo(comparisonLeft,
            { opacity: 0, x: -30 },
            {
                opacity: 1,
                x: 0,
                duration: 0.6,
                ease: 'power4.out',
                force3D: true,
                scrollTrigger: {
                    trigger: comparisonLeft,
                    start: 'top 90%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    if (comparisonRight) {
        gsap.fromTo(comparisonRight,
            { opacity: 0, x: 30 },
            {
                opacity: 1,
                x: 0,
                duration: 0.6,
                ease: 'power4.out',
                force3D: true,
                scrollTrigger: {
                    trigger: comparisonRight,
                    start: 'top 90%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    // Overlay animations
    const leftOverlay = document.getElementById('left-overlay');
    const rightOverlay = document.getElementById('right-overlay');

    if (leftOverlay) {
        gsap.to(leftOverlay, {
            width: '100%',
            duration: 1,
            ease: 'power3.out',
            scrollTrigger: {
                trigger: comparisonLeft,
                start: 'top 90%',
                toggleActions: 'play none none none'
            }
        });
    }

    if (rightOverlay) {
        gsap.to(rightOverlay, {
            width: '100%',
            duration: 1,
            ease: 'power3.out',
            scrollTrigger: {
                trigger: comparisonRight,
                start: 'top 90%',
                toggleActions: 'play none none none'
            }
        });
    }
}

function initArchitectureAnimations() {
    const arrows = document.querySelectorAll('[id^="arrow-"]');
    arrows.forEach((arrow, index) => {
        gsap.fromTo(arrow,
            { opacity: 0, scale: 0 },
            {
                opacity: 1,
                scale: 1,
                duration: 0.4,
                ease: 'back.out(1.4)',
                force3D: true,
                scrollTrigger: {
                    trigger: arrow,
                    start: 'top 90%',
                    toggleActions: 'play none none none'
                },
                delay: index * 0.05
            }
        );
    });

    const dividers = document.querySelectorAll('[id^="divider-"]');
    dividers.forEach((divider, index) => {
        gsap.fromTo(divider,
            { opacity: 0, scaleY: 0 },
            {
                opacity: 1,
                scaleY: 1,
                duration: 0.5,
                ease: 'power3.out',
                force3D: true,
                scrollTrigger: {
                    trigger: divider,
                    start: 'top 90%',
                    toggleActions: 'play none none none'
                },
                delay: index * 0.05
            }
        );
    });
}

