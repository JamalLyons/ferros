import '../styles/style.css';

// Register GSAP ScrollTrigger plugin
gsap.registerPlugin(ScrollTrigger);

// Performance: Set default GPU acceleration
gsap.config({ force3D: true });

// Initialize animations when DOM is loaded
document.addEventListener('DOMContentLoaded', () => {
    // Check if intro should be skipped (24 hour expiration)
    const lastIntroTime = localStorage.getItem('ferros_intro_last_shown');
    const oneDayInMs = 24 * 60 * 60 * 1000; // 24 hours in milliseconds
    const now = Date.now();

    // Show intro if never shown before, or if 24 hours have passed
    const shouldShowIntro = !lastIntroTime || (now - parseInt(lastIntroTime)) > oneDayInMs;

    if (shouldShowIntro) {
        initCinematicIntro();
    } else {
        initParticles();
        initHeroAnimations();
    }

    initScrollAnimations();
    initFeatureHovers();
    initDebuggerSimulation();
    initFloatingNavigation();
    initDepthEnhancements();
    initRoadmap();
});

// Cinematic Intro Animation
function initCinematicIntro() {
    const introOverlay = document.createElement('div');
    introOverlay.id = 'intro-overlay';
    introOverlay.className = 'fixed inset-0 z-[9999] bg-gradient-to-br from-[#0d0d0d] via-[#1a1a1a] to-[#0d0d0d]';
    document.body.appendChild(introOverlay);

    const spark = document.createElement('div');
    spark.id = 'intro-spark';
    spark.className = 'absolute w-2 h-2 bg-[#d3752b] rounded-full shadow-[0_0_20px_#d3752b]';
    introOverlay.appendChild(spark);

    const logoContainer = document.createElement('div');
    logoContainer.id = 'intro-logo-container';
    logoContainer.className = 'absolute inset-0 flex items-center justify-center opacity-0';
    const logo = document.createElement('img');
    logo.src = '/ferros-logo.png';
    logo.className = 'w-32 h-32 rounded-full object-cover border-2 border-[#d3752b]';
    logoContainer.appendChild(logo);
    introOverlay.appendChild(logoContainer);

    const tl = gsap.timeline();

    // Spark travels across screen
    tl.set(spark, { x: '-100vw', y: '50vh' })
        .to(spark, {
            x: '50vw',
            y: '50vh',
            duration: 1.2,
            ease: 'power2.inOut'
        })
        .to(spark, {
            scale: 8,
            opacity: 0.8,
            duration: 0.3,
            ease: 'power2.out'
        })
        // Flash effect
        .to(introOverlay, {
            backgroundColor: '#d3752b',
            duration: 0.1,
            ease: 'power2.out'
        })
        .to(introOverlay, {
            backgroundColor: '#0d0d0d',
            duration: 0.2,
            ease: 'power2.out'
        })
        // Logo appears with cooling effect
        .to(logoContainer, {
            opacity: 1,
            scale: 1,
            duration: 0.6,
            ease: 'back.out(1.4)'
        })
        .to(logo, {
            filter: 'brightness(1.2)',
            duration: 0.3
        })
        .to(logo, {
            filter: 'brightness(1)',
            duration: 0.5
        })
        // Fade out overlay
        .to(introOverlay, {
            opacity: 0,
            duration: 0.8,
            ease: 'power2.in'
        })
        .call(() => {
            // Store current timestamp for 24-hour expiration
            localStorage.setItem('ferros_intro_last_shown', Date.now().toString());
            introOverlay.remove();
            initParticles();
            initHeroAnimations();
        });
}

// Create floating particles in hero section (optimized for mobile)
function initParticles() {
    const particlesContainer = document.getElementById('particles');
    if (!particlesContainer) return;

    const isMobile = window.innerWidth < 768;
    const particleCount = isMobile ? 15 : 30;

    for (let i = 0; i < particleCount; i++) {
        const particle = document.createElement('div');
        particle.className = 'particle';

        particle.style.left = `${Math.random() * 100}%`;
        particle.style.top = `${Math.random() * 100}%`;

        const delay = Math.random() * 8;
        const duration = 8 + Math.random() * 4;
        particle.style.animationDelay = `${delay}s`;
        particle.style.animationDuration = `${duration}s`;

        particlesContainer.appendChild(particle);
    }
}

// Hero section animations (optimized)
function initHeroAnimations() {
    const ironRing = document.getElementById('iron-ring');
    if (ironRing) {
        const isMobile = window.innerWidth < 768;
        gsap.to(ironRing, {
            rotation: 360,
            duration: 30,
            repeat: -1,
            ease: 'none',
            force3D: true
        });

        // Scale down on mobile
        if (isMobile) {
            gsap.set(ironRing, { scale: 0.7 });
        }
    }

    const logo = document.getElementById('logo');
    if (logo) {
        gsap.to(logo, {
            opacity: 1,
            scale: 1,
            duration: 0.8, // Reduced from 1.5
            ease: 'power4.out', // Sharper easing
            delay: 0.2,
            force3D: true
        });
    }

    const tagline = document.getElementById('tagline');
    if (tagline) {
        gsap.to(tagline, {
            opacity: 1,
            y: 0,
            duration: 0.7, // Reduced from 1.2
            ease: 'power4.out',
            delay: 0.5,
            force3D: true
        });
    }

    const subtitle = document.getElementById('subtitle');
    if (subtitle) {
        gsap.to(subtitle, {
            opacity: 1,
            y: 0,
            duration: 0.7, // Reduced from 1.2
            ease: 'power4.out',
            delay: 0.7,
            force3D: true
        });
    }

    const scrollIndicator = document.getElementById('scroll-indicator');
    if (scrollIndicator) {
        gsap.to(scrollIndicator, {
            opacity: 1,
            duration: 0.5,
            delay: 1.2,
            force3D: true
        });
    }
}

// Optimized scroll animations using ScrollTrigger.batch()
function initScrollAnimations() {
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
                    duration: 0.6, // Reduced from 1
                    ease: 'power4.out',
                    force3D: true,
                    scrollTrigger: {
                        trigger: el,
                        start: 'top 90%', // Trigger earlier
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

    // Comparison section with faster animations
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

    // Architecture arrows and dividers
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

// Feature banner hover effects (optimized)
function initFeatureHovers() {
    const featureBanners = document.querySelectorAll('[data-feature]');

    featureBanners.forEach((banner) => {
        const glowId = `feature-glow-${banner.dataset.feature}`;
        const glow = document.getElementById(glowId);

        if (glow) {
            banner.addEventListener('mouseenter', () => {
                gsap.to(glow, {
                    opacity: 1,
                    duration: 0.2,
                    ease: 'power2.out'
                });
            });

            banner.addEventListener('mouseleave', () => {
                gsap.to(glow, {
                    opacity: 0,
                    duration: 0.2,
                    ease: 'power2.out'
                });
            });
        }
    });
}

// Live Debugger Simulation
function initDebuggerSimulation() {
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

// Floating Scroll Navigation
function initFloatingNavigation() {
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

// Depth & Grid Enhancements
function initDepthEnhancements() {
    // Hex pattern parallax
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

    // Oscillating vignette
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

    // Architecture wireframe grid
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

    // Crate connection lines animation
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

// Roadmap Visualization
function initRoadmap() {
    const roadmapSection = document.getElementById('roadmap');
    if (!roadmapSection) return;

    const roadmapTitle = document.getElementById('roadmap-title');
    if (roadmapTitle) {
        gsap.fromTo(roadmapTitle,
            { opacity: 0, y: 30 },
            {
                opacity: 1,
                y: 0,
                duration: 0.6,
                ease: 'power4.out',
                force3D: true,
                scrollTrigger: {
                    trigger: roadmapTitle,
                    start: 'top 90%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    const timeline = roadmapSection.querySelector('.roadmap-timeline');
    const milestones = roadmapSection.querySelectorAll('.roadmap-milestone');

    // Animate timeline on scroll
    if (timeline) {
        gsap.fromTo(timeline,
            { scaleX: 0 },
            {
                scaleX: 1,
                duration: 1,
                ease: 'power3.out',
                force3D: true,
                scrollTrigger: {
                    trigger: roadmapSection,
                    start: 'top 90%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    // Animate milestones
    milestones.forEach((milestone, index) => {
        gsap.fromTo(milestone,
            { opacity: 0, scale: 0 },
            {
                opacity: 1,
                scale: 1,
                duration: 0.5,
                ease: 'back.out(1.4)',
                force3D: true,
                scrollTrigger: {
                    trigger: milestone,
                    start: 'top 90%',
                    toggleActions: 'play none none none'
                },
                delay: index * 0.05
            }
        );

        // Pulsing animation for active milestones
        const node = milestone.querySelector('.milestone-node');
        if (node && node.classList.contains('bg-[#d3752b]')) {
            gsap.to(node, {
                scale: 1.2,
                duration: 1.5,
                repeat: -1,
                yoyo: true,
                ease: 'sine.inOut',
                force3D: true
            });
        }
    });
}

// Parallax effect for hero section (optimized)
let ticking = false;
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

// Add ScrollTo plugin if not already loaded
if (typeof gsap.registerPlugin !== 'undefined') {
    // ScrollTo plugin would need to be loaded separately
    // For now, using native smooth scroll
}
