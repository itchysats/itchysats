import { Alert, AlertDescription, AlertIcon, AlertTitle, CloseButton, Spacer } from "@chakra-ui/react";
import * as React from "react";

type IncompatibleWarningProps = {
    onClose: () => void;
};

export default function IncompatibleWarning({ onClose }: IncompatibleWarningProps) {
    return (
        <Alert status="warning">
            <AlertIcon />
            <AlertTitle>Your daemon is incompatible with the connected maker!</AlertTitle>
            <AlertDescription>
                Running an incompatible version can result in unexpected behaviour. Ensure compatibility by updating to
                the latest ItchySats version!
            </AlertDescription>
            <Spacer />
            <CloseButton alignSelf="flex-start" position="relative" right={-1} top={-1} onClick={onClose} />
        </Alert>
    );
}
