import '../styles/style.css';

// Register GSAP ScrollTrigger plugin
gsap.registerPlugin(ScrollTrigger);

// Initialize animations when DOM is loaded
document.addEventListener('DOMContentLoaded', () => {
    initParticles();
    initHeroAnimations();
    initScrollAnimations();
    initFeatureHovers();
});

// Create floating particles in hero section
function initParticles() {
    const particlesContainer = document.getElementById('particles');
    if (!particlesContainer) return;

    const particleCount = 30;

    for (let i = 0; i < particleCount; i++) {
        const particle = document.createElement('div');
        particle.className = 'particle';

        // Random position
        particle.style.left = `${Math.random() * 100}%`;
        particle.style.top = `${Math.random() * 100}%`;

        // Random animation delay and duration
        const delay = Math.random() * 8;
        const duration = 8 + Math.random() * 4;
        particle.style.animationDelay = `${delay}s`;
        particle.style.animationDuration = `${duration}s`;

        particlesContainer.appendChild(particle);
    }
}

// Hero section animations
function initHeroAnimations() {
    // Rotate iron ring continuously
    const ironRing = document.getElementById('iron-ring');
    if (ironRing) {
        gsap.to(ironRing, {
            rotation: 360,
            duration: 30,
            repeat: -1,
            ease: 'none'
        });
    }

    // Logo reveal
    const logo = document.getElementById('logo');
    if (logo) {
        gsap.to(logo, {
            opacity: 1,
            scale: 1,
            duration: 1.5,
            ease: 'power3.out',
            delay: 0.3
        });
    }

    // Tagline reveal
    const tagline = document.getElementById('tagline');
    if (tagline) {
        gsap.to(tagline, {
            opacity: 1,
            y: 0,
            duration: 1.2,
            ease: 'power3.out',
            delay: 0.8
        });
    }

    // Subtitle reveal
    const subtitle = document.getElementById('subtitle');
    if (subtitle) {
        gsap.to(subtitle, {
            opacity: 1,
            y: 0,
            duration: 1.2,
            ease: 'power3.out',
            delay: 1.2
        });
    }

    // Scroll indicator
    const scrollIndicator = document.getElementById('scroll-indicator');
    if (scrollIndicator) {
        gsap.to(scrollIndicator, {
            opacity: 1,
            duration: 1,
            delay: 2
        });
    }
}

// Scroll-triggered animations
function initScrollAnimations() {
    // "What Makes Ferros Different" section
    const differentTitle = document.getElementById('different-title');
    if (differentTitle) {
        gsap.fromTo(differentTitle,
            { opacity: 0, y: 50 },
            {
                opacity: 1,
                y: 0,
                duration: 1,
                ease: 'power3.out',
                scrollTrigger: {
                    trigger: differentTitle,
                    start: 'top 80%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    // Different cards animation
    const cards = document.querySelectorAll('#different-cards > div');
    cards.forEach((card, index) => {
        gsap.fromTo(card,
            { opacity: 0, x: index < 2 ? -100 : 100 },
            {
                opacity: 1,
                x: 0,
                duration: 0.8,
                ease: 'power3.out',
                scrollTrigger: {
                    trigger: card,
                    start: 'top 85%',
                    toggleActions: 'play none none none'
                },
                delay: index * 0.1
            }
        );
    });

    // Comparison section
    const comparisonLeft = document.getElementById('comparison-left');
    const comparisonRight = document.getElementById('comparison-right');

    if (comparisonLeft) {
        gsap.fromTo(comparisonLeft,
            { opacity: 0, x: -50 },
            {
                opacity: 1,
                x: 0,
                duration: 1,
                ease: 'power3.out',
                scrollTrigger: {
                    trigger: comparisonLeft,
                    start: 'top 80%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    if (comparisonRight) {
        gsap.fromTo(comparisonRight,
            { opacity: 0, x: 50 },
            {
                opacity: 1,
                x: 0,
                duration: 1,
                ease: 'power3.out',
                scrollTrigger: {
                    trigger: comparisonRight,
                    start: 'top 80%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    // Animate overlay widths on scroll
    const leftOverlay = document.getElementById('left-overlay');
    const rightOverlay = document.getElementById('right-overlay');

    if (leftOverlay) {
        gsap.to(leftOverlay, {
            width: '100%',
            duration: 1.5,
            ease: 'power2.out',
            scrollTrigger: {
                trigger: comparisonLeft,
                start: 'top 80%',
                toggleActions: 'play none none none'
            }
        });
    }

    if (rightOverlay) {
        gsap.to(rightOverlay, {
            width: '100%',
            duration: 1.5,
            ease: 'power2.out',
            scrollTrigger: {
                trigger: comparisonRight,
                start: 'top 80%',
                toggleActions: 'play none none none'
            }
        });
    }

    // Features section title
    const featuresTitle = document.getElementById('features-title');
    if (featuresTitle) {
        gsap.fromTo(featuresTitle,
            { opacity: 0, y: 50 },
            {
                opacity: 1,
                y: 0,
                duration: 1,
                ease: 'power3.out',
                scrollTrigger: {
                    trigger: featuresTitle,
                    start: 'top 80%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    // Feature banners
    const featureBanners = document.querySelectorAll('[data-feature]');
    featureBanners.forEach((banner, index) => {
        gsap.fromTo(banner,
            { opacity: 0, y: 50 },
            {
                opacity: 1,
                y: 0,
                duration: 0.8,
                ease: 'power3.out',
                scrollTrigger: {
                    trigger: banner,
                    start: 'top 85%',
                    toggleActions: 'play none none none'
                },
                delay: index * 0.1
            }
        );
    });

    // Architecture section
    const architectureTitle = document.getElementById('architecture-title');
    if (architectureTitle) {
        gsap.fromTo(architectureTitle,
            { opacity: 0, y: 50 },
            {
                opacity: 1,
                y: 0,
                duration: 1,
                ease: 'power3.out',
                scrollTrigger: {
                    trigger: architectureTitle,
                    start: 'top 80%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    // Crate cards animation
    const crateCards = document.querySelectorAll('.crate-card');
    crateCards.forEach((card, index) => {
        gsap.fromTo(card,
            { opacity: 0, scale: 0 },
            {
                opacity: 1,
                scale: 1,
                duration: 0.6,
                ease: 'back.out(1.7)',
                scrollTrigger: {
                    trigger: card,
                    start: 'top 85%',
                    toggleActions: 'play none none none'
                },
                delay: index * 0.1
            }
        );
    });

    // Arrows animation
    const arrows = document.querySelectorAll('[id^="arrow-"]');
    arrows.forEach((arrow, index) => {
        gsap.fromTo(arrow,
            { opacity: 0, scale: 0 },
            {
                opacity: 1,
                scale: 1,
                duration: 0.5,
                ease: 'power2.out',
                scrollTrigger: {
                    trigger: arrow,
                    start: 'top 85%',
                    toggleActions: 'play none none none'
                },
                delay: index * 0.1
            }
        );
    });

    // Dividers animation
    const dividers = document.querySelectorAll('[id^="divider-"]');
    dividers.forEach((divider, index) => {
        gsap.fromTo(divider,
            { opacity: 0, scaleY: 0 },
            {
                opacity: 1,
                scaleY: 1,
                duration: 0.6,
                ease: 'power2.out',
                scrollTrigger: {
                    trigger: divider,
                    start: 'top 85%',
                    toggleActions: 'play none none none'
                },
                delay: index * 0.1
            }
        );
    });

    // Mission statement
    const missionContent = document.getElementById('mission-content');
    if (missionContent) {
        gsap.fromTo(missionContent,
            { opacity: 0, y: 30 },
            {
                opacity: 1,
                y: 0,
                duration: 1,
                ease: 'power3.out',
                scrollTrigger: {
                    trigger: missionContent,
                    start: 'top 80%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    // CTA section
    const ctaContent = document.getElementById('cta-content');
    if (ctaContent) {
        gsap.fromTo(ctaContent,
            { opacity: 0, y: 30 },
            {
                opacity: 1,
                y: 0,
                duration: 1,
                ease: 'power3.out',
                scrollTrigger: {
                    trigger: ctaContent,
                    start: 'top 80%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }
}

// Feature banner hover effects
function initFeatureHovers() {
    const featureBanners = document.querySelectorAll('[data-feature]');

    featureBanners.forEach((banner) => {
        const glowId = `feature-glow-${banner.dataset.feature}`;
        const glow = document.getElementById(glowId);

        if (glow) {
            banner.addEventListener('mouseenter', () => {
                gsap.to(glow, {
                    opacity: 1,
                    duration: 0.3,
                    ease: 'power2.out'
                });
            });

            banner.addEventListener('mouseleave', () => {
                gsap.to(glow, {
                    opacity: 0,
                    duration: 0.3,
                    ease: 'power2.out'
                });
            });
        }
    });
}

// Parallax effect for hero section
window.addEventListener('scroll', () => {
    const scrolled = window.pageYOffset;
    const hero = document.getElementById('hero');
    const particles = document.getElementById('particles');

    if (hero && scrolled < hero.offsetHeight) {
        const parallaxSpeed = 0.5;
        if (particles) {
            particles.style.transform = `translateY(${scrolled * parallaxSpeed}px)`;
        }
    }
});
