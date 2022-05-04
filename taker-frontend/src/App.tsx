import {
    Accordion,
    AccordionButton,
    AccordionIcon,
    AccordionItem,
    AccordionPanel,
    Box,
    Button,
    ButtonGroup,
    Center,
    HStack,
    Text,
    useColorModeValue,
    useToast,
    VStack,
} from "@chakra-ui/react";
import dayjs from "dayjs";
import isBetween from "dayjs/plugin/isBetween";
import relativeTime from "dayjs/plugin/relativeTime";
import utc from "dayjs/plugin/utc";
import "intro.js/introjs.css";
import * as React from "react";
import { useEffect, useState } from "react";
import { Navigate, Outlet, Route, Routes, useLocation } from "react-router-dom";
import { Link as ReachLink } from "react-router-dom";
import useWebSocket from "react-use-websocket";
import Footer from "./components/Footer";
import History from "./components/History";
import Nav from "./components/NavBar";
import { Tour } from "./components/Tour";
import Trade from "./components/Trade";
import { Wallet } from "./components/Wallet";
import { BXBTData, Cfd, ConnectionStatus, intoCfd, intoMakerOffer, isClosed, MakerOffer, WalletInfo } from "./types";
import { useEventSource } from "./useEventSource";
import useLatestEvent from "./useLatestEvent";

export interface Offer {
    id?: string;
    price?: number;
    marginPerLot?: number;
    initialFundingFeePerLot?: number;
    liquidationPrice?: number;
    fundingRateAnnualized?: number;
    fundingRateHourly?: number;

    // defaulted for display purposes
    minQuantity: number;
    maxQuantity: number;
    lotSize: number;
    leverage: number;
}

// TODO: Evaluate moving these globals into the theme to make them accessible through that
export const VIEWPORT_WIDTH = 1000;
export const FOOTER_HEIGHT = 50;
export const HEADER_HEIGHT = 100;
export const VIEWPORT_WIDTH_PX = VIEWPORT_WIDTH + "px";
export const BG_LIGHT = "gray.50";
export const BG_DARK = "gray.800";
export const FAQ_URL = "http://faq.itchysats.network";

export const App = () => {
    const toast = useToast();

    let [referencePrice, setReferencePrice] = useState<number>();

    useWebSocket("wss://www.bitmex.com/realtime?subscribe=instrument:.BXBT", {
        shouldReconnect: () => true,
        onMessage: (message) => {
            const data: BXBTData[] = JSON.parse(message.data).data;
            if (data && data[0]?.markPrice) {
                setReferencePrice(data[0].markPrice);
            }
        },
    });

    const [source, isConnected] = useEventSource("/api/feed");
    const walletInfo = useLatestEvent<WalletInfo>(source, "wallet");

    const makerLong = useLatestEvent<MakerOffer>(source, "long_offer", intoMakerOffer);
    const makerShort = useLatestEvent<MakerOffer>(source, "short_offer", intoMakerOffer);

    const shortOffer = makerOfferToTakerOffer(makerLong);
    const longOffer = makerOfferToTakerOffer(makerShort);

    function makerOfferToTakerOffer(offer: MakerOffer | null): Offer {
        if (offer) {
            return {
                id: offer.id,
                initialFundingFeePerLot: offer.initial_funding_fee_per_lot,
                marginPerLot: offer.margin_per_lot,
                price: offer.price,
                liquidationPrice: offer.liquidation_price,
                fundingRateAnnualized: offer.funding_rate_annualized_percent,
                fundingRateHourly: toFixedNumber(offer.funding_rate_hourly_percent, 5),
                minQuantity: offer.min_quantity,
                maxQuantity: offer.max_quantity,
                lotSize: offer.lot_size,
                leverage: offer.leverage,
            };
        }

        return {
            minQuantity: 0,
            maxQuantity: 0,
            lotSize: 100,
            leverage: 2,
        };
    }

    function toFixedNumber(n: number, digits: number): number {
        // Conversion of the number into Number needed to avoid "toFixed is not a function" errors
        return Number.parseFloat(Number(n).toFixed(digits));
    }

    const cfdsOrUndefined = useLatestEvent<Cfd[]>(source, "cfds", intoCfd);
    let cfds = cfdsOrUndefined ? cfdsOrUndefined! : [];
    const connectedToMakerOrUndefined = useLatestEvent<ConnectionStatus>(source, "maker_status");
    const connectedToMaker = connectedToMakerOrUndefined ? connectedToMakerOrUndefined : { online: false };

    dayjs.extend(relativeTime);
    dayjs.extend(utc);
    dayjs.extend(isBetween);

    // TODO: Eventually this should be calculated with what the maker defines in the offer, for now we assume full hour
    const nextFullHour = dayjs().utc().minute(0).add(1, "hour");

    // TODO: this condition is a bit weird now
    const nextFundingEvent = longOffer || shortOffer ? dayjs().to(nextFullHour) : null;

    useEffect(() => {
        const id = "connection-toast";
        if (!isConnected && !toast.isActive(id)) {
            toast({
                id,
                status: "error",
                isClosable: true,
                duration: null,
                position: "bottom",
                title: "Connection error!",
                description: "Please ensure your daemon is running. Then refresh the page.",
            });
        } else if (isConnected && toast.isActive(id)) {
            toast.close(id);
        }
    }, [toast, isConnected]);

    useEffect(() => {
        const id = "maker-connection-toast";
        if (connectedToMakerOrUndefined && !connectedToMakerOrUndefined.online && !toast.isActive(id)) {
            toast({
                id,
                status: "warning",
                isClosable: true,
                duration: null,
                position: "bottom",
                title: "No maker!",
                description: "You are not connected to any maker. Functionality may be limited",
            });
        } else if (connectedToMakerOrUndefined && connectedToMakerOrUndefined.online && toast.isActive(id)) {
            toast.close(id);
        }
    }, [toast, connectedToMakerOrUndefined]);

    return (
        <>
            <Tour />

            <Nav
                walletInfo={walletInfo}
                connectedToMaker={connectedToMaker}
                nextFundingEvent={nextFundingEvent}
                referencePrice={referencePrice}
            />
            <Center>
                <Box
                    maxWidth={(VIEWPORT_WIDTH + 200) + "px"}
                    width={"100%"}
                    bgGradient={useColorModeValue(
                        "linear(to-r, white 5%, gray.800, white 95%)",
                        "linear(to-r, gray.800 5%, white, gray.800 95%)",
                    )}
                >
                    <Center>
                        <Box
                            textAlign="center"
                            padding={3}
                            bg={useColorModeValue(BG_LIGHT, BG_DARK)}
                            maxWidth={VIEWPORT_WIDTH_PX}
                            marginTop={`${HEADER_HEIGHT}px`}
                            minHeight={`calc(100vh - ${FOOTER_HEIGHT}px - ${HEADER_HEIGHT}px)`}
                            width={"100%"}
                        >
                            <Routes>
                                <Route path="/">
                                    <Route
                                        path="wallet"
                                        element={<Wallet walletInfo={walletInfo} />}
                                    />
                                    <Route
                                        element={
                                            // @ts-ignore: ts-lint thinks that {children} is missing but react router is taking care of this for us


                                                <PageLayout
                                                    cfds={cfds}
                                                    connectedToMaker={connectedToMaker}
                                                />

                                        }
                                    >
                                        <Route
                                            path="long"
                                            element={
                                                <Trade
                                                    offer={longOffer}
                                                    connectedToMaker={connectedToMaker}
                                                    walletBalance={walletInfo ? walletInfo.balance : 0}
                                                    isLong={true}
                                                />
                                            }
                                        />
                                        <Route
                                            path="short"
                                            element={
                                                <Trade
                                                    offer={shortOffer}
                                                    connectedToMaker={connectedToMaker}
                                                    walletBalance={walletInfo ? walletInfo.balance : 0}
                                                    isLong={false}
                                                />
                                            }
                                        />
                                    </Route>
                                    <Route index element={<Navigate to="long" />} />
                                </Route>
                                <Route
                                    path="/*"
                                    element={<Navigate to="long" />}
                                />
                            </Routes>
                        </Box>
                    </Center>
                </Box>
            </Center>
            <Footer />
        </>
    );
};

interface PageLayoutProps {
    cfds: Cfd[];
    connectedToMaker: ConnectionStatus;
}

function PageLayout({ cfds, connectedToMaker }: PageLayoutProps) {
    return (
        <VStack w={"100%"}>
            <NavigationButtons />
            <Outlet />
            <HistoryLayout cfds={cfds} connectedToMaker={connectedToMaker} />
        </VStack>
    );
}

interface HistoryLayoutProps {
    cfds: Cfd[];
    connectedToMaker: ConnectionStatus;
}

function HistoryLayout({ cfds, connectedToMaker }: HistoryLayoutProps) {
    const closedPositions = cfds.filter((cfd) => isClosed(cfd));

    return (
        <VStack padding={3} w={"100%"}>
            <History
                connectedToMaker={connectedToMaker}
                cfds={cfds.filter((cfd) => !isClosed(cfd))}
            />

            {closedPositions.length > 0
                && (
                    <Accordion allowToggle maxWidth={VIEWPORT_WIDTH_PX} width={"100%"}>
                        <AccordionItem>
                            <h2>
                                <AccordionButton>
                                    <AccordionIcon />
                                    <Box w={"100%"} textAlign="center">
                                        Show Closed Positions
                                    </Box>
                                    <AccordionIcon />
                                </AccordionButton>
                            </h2>
                            <AccordionPanel pb={4}>
                                <History
                                    cfds={closedPositions}
                                    connectedToMaker={connectedToMaker}
                                />
                            </AccordionPanel>
                        </AccordionItem>
                    </Accordion>
                )}
        </VStack>
    );
}

function NavigationButtons() {
    const location = useLocation();
    const isLongSelected = location.pathname.includes("long");
    const isShortSelected = !isLongSelected;

    const unSelectedButton = "transparent";
    const selectedButton = useColorModeValue("grey.400", "black.400");
    const buttonBorder = useColorModeValue("grey.400", "black.400");
    const buttonText = useColorModeValue("black", "white");

    return (
        <HStack>
            <Center>
                <ButtonGroup
                    padding="3"
                    spacing="6"
                    id={"longShortButtonSwitch"}
                >
                    <Button
                        as={ReachLink}
                        to="/long"
                        color={isLongSelected ? selectedButton : unSelectedButton}
                        bg={isLongSelected ? selectedButton : unSelectedButton}
                        border={buttonBorder}
                        isActive={isLongSelected}
                        size="lg"
                        h={10}
                        w={"40"}
                    >
                        <Text fontSize={"md"} color={buttonText}>Long BTC</Text>
                    </Button>
                    <Button
                        as={ReachLink}
                        to="/short"
                        color={isShortSelected ? selectedButton : unSelectedButton}
                        bg={isShortSelected ? selectedButton : unSelectedButton}
                        border={buttonBorder}
                        isActive={isShortSelected}
                        size="lg"
                        h={10}
                        w={"40"}
                    >
                        <Text fontSize={"md"} color={buttonText}>Short BTC</Text>
                    </Button>
                </ButtonGroup>
            </Center>
        </HStack>
    );
}
